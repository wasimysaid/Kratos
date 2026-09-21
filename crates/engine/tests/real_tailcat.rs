//! Rust Auth/EngineRuntime over a real Tailcat connection and local DERP.
//!
//! The DERP/STUN service is a test-only Go process. Unlike the Go unit tests,
//! this test drives the Rust Auth pairing protocol and managed adapter.

#[cfg(unix)]
struct FixtureProcess(std::process::Child);

#[cfg(unix)]
impl std::ops::Deref for FixtureProcess {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for FixtureProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let pid = self.0.id() as i32;
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rust_auth_pairing_and_engine_runtime_use_real_tailcat() {
    use sha2::Digest as _;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::Duration;
    use kratos_engine::{Auth, AuthConfig, Engine, EngineConfig, EngineProfile};

    use kratos_doc::{MessageRole, MessageStatus, QueueDeliveryGate, SessionCommandPayload};
    use kratos_proto::{RunRequest, SandboxLevel};

    use kratos_rpc::{memory_client, methods};

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf();
    let module = workspace.join("connectivity/tailcat");
    let go = std::env::var_os("GO")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let bundled = workspace.join(".research/tools/go/bin/go");
            bundled.is_file().then_some(bundled)
        })
        .unwrap_or_else(|| PathBuf::from("go"));

    let adapter = tempfile::tempdir().expect("adapter tempdir");
    let adapter_bin = adapter.path().join("kratos-tailcat");
    let build = Command::new(&go)
        .current_dir(&module)
        .args(["build", "-o"])
        .arg(&adapter_bin)
        .arg("./cmd/kratos-tailcat")
        .output()
        .expect("build Tailcat adapter");
    assert!(
        build.status.success(),
        "adapter build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let fixture = Command::new(&go)
        .env("KRATOS_TAILCAT_FIXTURE_SERVE", "1")
        .current_dir(&module)
        .args([
            "test",
            "./cmd/test-derp",
            "-run",
            "^TestTailcatFixture$",
            "-count=1",
            "-v",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .expect("start DERP fixture");

    let mut fixture = FixtureProcess(fixture);
    let stdout = fixture.stdout.take().expect("fixture stdout");
    let mut lines = BufReader::new(stdout).lines();
    let descriptor = tokio::task::spawn_blocking(move || {
        for line in lines.by_ref() {
            let line = line.expect("fixture output");
            if line.starts_with('{') && line.contains("map_url") {
                return serde_json::from_str::<serde_json::Value>(&line)
                    .expect("fixture descriptor");
            }
        }
        panic!("DERP fixture exited before descriptor")
    })
    .await
    .expect("fixture reader");
    let map_url = descriptor["map_url"].as_str().expect("map URL").to_owned();
    let cert_file = descriptor["cert_file"].as_str().expect("map CA").to_owned();
    // Go's HTTP client honors SSL_CERT_FILE. It is inherited by the managed
    // adapter subprocesses, while the test itself does not alter global roots.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &cert_file);
    }

    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let auth_a = Auth::open(AuthConfig {
        data_dir: a_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .unwrap();
    let initialized = auth_a
        .initialize_peer("integration-owner", Some(map_url.clone()))
        .await
        .expect("initialize real peer");
    assert!(
        initialized.hosting && initialized.connected,
        "host status: {initialized:?}"
    );
    let invitation = auth_a.invite().await.expect("real invitation");

    let auth_b = Auth::open(AuthConfig {
        data_dir: b_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .unwrap();
    auth_b
        .pair(&invitation)
        .await
        .expect("pair through real Tailcat");
    let paired = auth_b.peer_status().await;
    assert!(
        paired.connected && !paired.hosting,
        "paired status: {paired:?}"
    );
    assert_ne!(
        initialized.device_id, paired.device_id,
        "pairing must create a distinct device identity"
    );

    let runtime_a = Engine::assemble_runtime(
        &EngineConfig {
            data_dir: a_dir.path().to_owned(),
            ipc_port: 0,
            default_harness: kratos_proto::HarnessId::Mock,
        },
        auth_a.clone(),
        EngineProfile::synced(
            a_dir.path(),
            &initialized.profile_id.clone().expect("profile"),
            "user-a",
        ),
    )
    .await
    .expect("runtime A");
    let runtime_b = Engine::assemble_runtime(
        &EngineConfig {
            data_dir: b_dir.path().to_owned(),
            ipc_port: 0,
            default_harness: kratos_proto::HarnessId::Mock,
        },
        auth_b.clone(),
        EngineProfile::synced(
            b_dir.path(),
            &paired.profile_id.clone().expect("profile"),
            "user-b",
        ),
    )
    .await
    .expect("runtime B");
    assert_eq!(
        runtime_a.core().device_id,
        initialized.device_id.as_deref().expect("device id")
    );
    assert_eq!(
        runtime_b.core().device_id,
        paired.device_id.as_deref().expect("device id")
    );

    // Exercise the production Engine RPC, registry, DocHost, EngineChatSink,
    // ChatClient, and durable-peer path over the real Tailcat/DERP transport.
    let rpc_a = memory_client(runtime_a.core().rpc_service());
    rpc_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op":"createSpace", "spaceId":"real-space", "deviceId":initialized.device_id,
                "path":"/tmp/real-peer", "gitDetected":false
            }),
        )
        .await
        .expect("create real space");
    rpc_a
        .call(
            methods::MUTATE,
            serde_json::json!({"op":"createChat", "chatId":"real-chat", "spaceId":"real-space"}),
        )
        .await
        .expect("create real chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while runtime_b
        .core()
        .workspace
        .chat("real-chat")
        .ok()
        .flatten()
        .is_none()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "real chat did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let host_chat = runtime_a
        .core()
        .doc_host
        .open("real-chat")
        .expect("host chat");
    let viewer_chat = runtime_b
        .core()
        .doc_host
        .open("real-chat")
        .expect("viewer chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(host_chat.connected() && viewer_chat.connected()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "real chat relay did not connect"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Keep the host from legitimately promoting an idle queue row before
    // this transport assertion can observe it.
    runtime_a.core().doc_host.pause_all_queues();
    let queued_id = runtime_b
        .core()
        .doc_host
        .queue_message("real-chat", "queued over real Tailcat", vec![])
        .expect("queue over real Tailcat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !host_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|q| q.id == queued_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "real queue did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Exercise concurrent queue editing and removal on the real shared doc.
    let second_id = runtime_b
        .core()
        .doc_host
        .queue_message("real-chat", "edit me over Tailcat", vec![])
        .expect("queue second real message");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !host_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|q| q.id == second_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "second queue row did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let device_a = initialized.device_id.clone().expect("owner device");
    let device_b = paired.device_id.clone().expect("paired device");

    // Two independent RPC clients contend concurrently for the same host-side
    // lease. One must acquire it and the other must observe the lock.
    let rpc_edit_a = memory_client(runtime_a.core().rpc_service());
    let rpc_edit_b = memory_client(runtime_b.core().rpc_service());
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let barrier_a = barrier.clone();
    let barrier_b = barrier.clone();
    let id_a = second_id.clone();
    let id_b = second_id.clone();
    let owner = initialized.device_id.clone().expect("owner device");
    let task_a = tokio::spawn(async move {
        barrier_a.wait().await;
        rpc_edit_a
            .call(
                methods::BEGIN_QUEUED_MESSAGE_EDIT,
                serde_json::json!({"chatId":"real-chat","id":id_a,"editorDeviceId":owner,"editorInstanceId":"rpc-a"}),
            )
            .await
    });
    let device_b_for_task = device_b.clone();

    let device_a_for_task = device_a.clone();

    let task_b = tokio::spawn(async move {
        barrier_b.wait().await;
        rpc_edit_b
            .call(
                methods::BEGIN_QUEUED_MESSAGE_EDIT,
                serde_json::json!({"chatId":"real-chat","id":id_b,"editorDeviceId":device_b_for_task,"editorInstanceId":"rpc-b","targetDeviceId":device_a_for_task}),
            )
            .await
    });
    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a
        .expect("owner edit task")
        .expect("owner begin edit");
    let result_b = result_b
        .expect("viewer edit task")
        .expect("viewer begin edit");
    let acquired = [&result_a, &result_b]
        .into_iter()
        .filter(|result| result["outcome"] == "acquired")
        .count();
    assert_eq!(
        acquired, 1,
        "exactly one concurrent editor acquires the lease"
    );
    assert!(
        result_a["outcome"] == "locked" || result_b["outcome"] == "locked",
        "losing concurrent editor must observe locked outcome: A={result_a} B={result_b}"
    );
    let begin = if result_a["outcome"] == "acquired" {
        result_a
    } else {
        result_b
    };
    let lease_id = begin["leaseId"].as_str().expect("real lease id").to_owned();
    let base_hash = begin["baseTextHash"]
        .as_str()
        .expect("real base hash")
        .to_owned();
    let finisher = memory_client(runtime_a.core().rpc_service());
    let renewed = finisher
        .call(
            methods::RENEW_QUEUED_MESSAGE_EDIT,
            serde_json::json!({"chatId":"real-chat","id":second_id,"leaseId":lease_id}),
        )
        .await
        .expect("renew real edit");
    assert_eq!(renewed["outcome"], "renewed");
    finisher
        .call(
            methods::FINISH_QUEUED_MESSAGE_EDIT,
            serde_json::json!({"chatId":"real-chat","id":second_id,"leaseId":lease_id,"action":"commit","text":"edited concurrently over Tailcat","expectedTextHash":base_hash}),
        )
        .await
        .expect("finish real edit");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !viewer_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|q| q.id == second_id && q.text == "edited concurrently over Tailcat")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "edited queue row did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let removed_id = runtime_b
        .core()
        .doc_host
        .queue_message("real-chat", "remove over Tailcat", vec![])
        .expect("queue removal row");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !host_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|q| q.id == removed_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "removal row did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        runtime_a
            .core()
            .doc_host
            .remove_queued_message("real-chat", &removed_id)
            .await
            .expect("remove real row")
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while viewer_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|q| q.id == removed_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "removed queue row remained on viewer"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Re-freeze after the edit/remove RPCs, which intentionally wake queue
    // delivery. Let any already-dispatched turn settle, then create a fresh row
    // whose lease is guaranteed to remain pending across the restart.
    runtime_a.core().doc_host.pause_all_queues();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while runtime_a.core().sessions.turn_in_flight("real-chat") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pre-restart queue turn did not settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let restart_queue_id = runtime_b
        .core()
        .doc_host
        .queue_message("real-chat", "persist lease across full restart", vec![])
        .expect("queue restart lease row");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !host_chat
        .doc()
        .read_queue()
        .unwrap_or_default()
        .iter()
        .any(|row| row.id == restart_queue_id)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "restart lease row did not converge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Install the gate before the Run command wakes queue delivery. Admission
    // and persistence are both observed over the remote viewer copy.
    let lease = memory_client(runtime_a.core().rpc_service())
        .call(
            methods::BEGIN_QUEUED_MESSAGE_EDIT,
            serde_json::json!({"chatId":"real-chat","id":restart_queue_id,"editorDeviceId":device_a,"editorInstanceId":"restart-owner"}),
        )
        .await
        .expect("acquire restart lease");
    assert_eq!(lease["outcome"], "acquired");
    let restart_lease_id = lease["leaseId"]
        .as_str()
        .expect("restart lease id")
        .to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let lease_reached_viewer = viewer_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .find(|row| row.id == restart_queue_id)
            .and_then(|row| row.delivery_gate.as_ref())
            .is_some_and(|gate| {
                matches!(gate, QueueDeliveryGate::Editing { lease_id, .. } if lease_id == &restart_lease_id)
            });
        if lease_reached_viewer {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "restart lease did not converge before shutdown"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Execute one durable command before the crash boundary and retain its exact
    // accepted row. Replaying that row after both runtimes restart must hit the
    // processed-command ledger rather than execute the harness twice.
    let completed_before_restart_command = host_chat
        .doc()
        .read_entries()
        .unwrap_or_default()
        .iter()
        .filter(|entry| {
            entry.role == MessageRole::Assistant && entry.status == Some(MessageStatus::Complete)
        })
        .count();

    let accepted_command_id = runtime_b
        .core()
        .doc_host
        .queue_command(
            "real-chat",
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: "execute exactly once across the real restart".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "~".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: Vec::new(),
                    worktree: None,
                    resume: None,
                },
                message_id: "real-restart-user".into(),
            },
        )
        .expect("queue durable restart command");
    let accepted_command = viewer_chat
        .doc()
        .read_commands()
        .expect("read accepted command")
        .into_iter()
        .find(|entry| entry.id == accepted_command_id)
        .expect("accepted command row");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let entries = host_chat.doc().read_entries().unwrap_or_default();
        let accepted_user_message = entries
            .iter()
            .any(|entry| entry.id == "real-restart-user" && entry.role == MessageRole::User);
        if accepted_user_message && !runtime_a.core().sessions.turn_in_flight("real-chat") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "durable restart command did not execute and settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let execution_count_before_restart = host_chat
        .doc()
        .read_entries()
        .unwrap_or_default()
        .iter()
        .filter(|entry| {
            entry.role == MessageRole::Assistant && entry.status == Some(MessageStatus::Complete)
        })
        .count();
    assert!(
        execution_count_before_restart > completed_before_restart_command,
        "accepted command produced no completed execution"
    );

    // Stop the viewer first, write while it is offline, then stop the actual
    // executor and the Auth-owned durable peer. Dropping every retained
    // handle/RPC/Auth clone is the app-restart boundary; sign_out is
    // deliberately not used because the on-disk device identities must remain.
    let viewer_endpoint_before = auth_b
        .peer_endpoint()
        .expect("viewer endpoint before restart");

    runtime_b.shutdown().await;
    drop(viewer_chat);
    drop(runtime_b);
    let offline_id = runtime_a
        .core()
        .doc_host
        .queue_message("real-chat", "offline replay over Tailcat", vec![])
        .expect("queue while viewer offline");
    let profile_id = initialized.profile_id.clone().expect("profile");
    let owner_endpoint_before = auth_a
        .peer_endpoint()
        .expect("owner endpoint before restart");
    runtime_a.shutdown().await;
    drop(rpc_a);
    drop(finisher);
    drop(host_chat);
    drop(runtime_a);

    auth_b.shutdown_peer_preserving_session().await;
    auth_a.shutdown_peer_preserving_session().await;
    drop(auth_b);
    drop(auth_a);

    let unavailable_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let probe = reqwest::Client::new();
        let owner_down = probe
            .get(format!("{owner_endpoint_before}/pair/devices"))
            .send()
            .await
            .is_err();
        let viewer_down = probe
            .get(format!("{viewer_endpoint_before}/pair/challenge"))
            .send()
            .await
            .is_err();
        if owner_down && viewer_down {
            break;
        }
        assert!(
            tokio::time::Instant::now() < unavailable_deadline,
            "old peer endpoints remained reachable after full shutdown: owner_down={owner_down} viewer_down={viewer_down}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let auth_a = Auth::open(AuthConfig {
        data_dir: a_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .expect("reopen owner auth from disk");
    let auth_b = Auth::open(AuthConfig {
        data_dir: b_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .expect("reopen viewer auth from disk");
    assert_eq!(auth_a.device_id().as_deref(), Some(device_a.as_str()));
    assert_eq!(auth_b.device_id().as_deref(), Some(device_b.as_str()));
    assert_eq!(auth_a.profile_id().as_deref(), Some(profile_id.as_str()));
    assert_eq!(auth_b.profile_id().as_deref(), Some(profile_id.as_str()));
    let restart_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        auth_a.resume().await;
        if auth_a.peer_status().await.connected {
            break;
        }
        assert!(
            tokio::time::Instant::now() < restart_deadline,
            "owner peer did not release/rebind its durable listener: {:?}",
            auth_a.peer_status().await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    loop {
        auth_b.resume().await;
        if auth_b.peer_status().await.connected {
            break;
        }
        assert!(
            tokio::time::Instant::now() < restart_deadline,
            "viewer adapter did not release/rebind its durable listener: {:?}",
            auth_b.peer_status().await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let owner_status = auth_a.peer_status().await;
    let viewer_status = auth_b.peer_status().await;
    assert!(
        owner_status.hosting && owner_status.connected,
        "{owner_status:?}"
    );
    assert!(
        !viewer_status.hosting && viewer_status.connected,
        "{viewer_status:?}"
    );

    let runtime_a = Engine::assemble_runtime(
        &EngineConfig {
            data_dir: a_dir.path().to_owned(),
            ipc_port: 0,
            default_harness: kratos_proto::HarnessId::Mock,
        },
        auth_a.clone(),
        EngineProfile::synced(a_dir.path(), &profile_id, "user-a"),
    )
    .await
    .expect("reopen executor runtime and durable peer");
    let runtime_b = Engine::assemble_runtime(
        &EngineConfig {
            data_dir: b_dir.path().to_owned(),
            ipc_port: 0,
            default_harness: kratos_proto::HarnessId::Mock,
        },
        auth_b.clone(),
        EngineProfile::synced(b_dir.path(), &profile_id, "user-b"),
    )
    .await
    .expect("reopen viewer runtime after full peer restart");
    assert_eq!(runtime_a.core().device_id, device_a);
    assert_eq!(runtime_b.core().device_id, device_b);

    let host_chat = runtime_a
        .core()
        .doc_host
        .open("real-chat")
        .expect("reopened executor chat");
    let reopened_chat = runtime_b
        .core()
        .doc_host
        .open("real-chat")
        .expect("reopened viewer chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let queue = reopened_chat.doc().read_queue().unwrap_or_default();
        let offline_replayed = queue.iter().any(|row| row.id == offline_id);
        let lease_recovered = queue
            .iter()
            .find(|row| row.id == restart_queue_id)
            .and_then(|row| row.delivery_gate.as_ref())
            .is_some_and(|gate| {
                matches!(gate, QueueDeliveryGate::Editing { lease_id, .. } if lease_id == &restart_lease_id)
            });
        if host_chat.connected() && reopened_chat.connected() && offline_replayed && lease_recovered
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "full restart did not recover offline queue row and edit lease"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    host_chat
        .doc()
        .queue_command(&accepted_command)
        .expect("replay exact accepted command row after restart");
    assert_eq!(
        runtime_a
            .core()
            .doc_host
            .ingest_relayed_command("real-chat", accepted_command.clone())
            .await
            .expect("replay exact accepted relay command"),
        "duplicate"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let entries = host_chat.doc().read_entries().expect("restart entries");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Complete)
            })
            .count(),
        execution_count_before_restart,
        "replayed accepted command executed a second time after full restart"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.id == "real-restart-user" && entry.role == MessageRole::User)
            .count(),
        1,
        "replayed command duplicated its client-minted user message"
    );

    // Upload through the sender's local Tailcat adapter to the canonical target
    // device. After commit, stop both sender EngineRuntime and sender Auth so
    // neither its adapter nor loopback listener can participate in the fetch.
    let sender_token = auth_b.access_token().await.expect("sender token");
    let sender_endpoint = auth_b.peer_endpoint().expect("sender peer endpoint");
    let target_endpoint = auth_a.peer_endpoint().expect("target peer endpoint");
    let expected_digest = format!("{:x}", sha2::Sha256::digest(b"real Tailcat attachment"));
    let bytes = b"real Tailcat attachment";
    let client = reqwest::Client::new();
    client
        .post(format!("{sender_endpoint}/attachment/real-upload"))
        .bearer_auth(&sender_token)
        .json(&serde_json::json!({"targetDevice":device_a,"fileName":"real.txt","length":bytes.len(),"digest":expected_digest}))
        .send()
        .await
        .expect("attachment begin over sender Tailcat")
        .error_for_status()
        .expect("attachment begin status");
    client
        .put(format!(
            "{sender_endpoint}/attachment/real-upload/chunk?targetDevice={device_a}&offset=0"
        ))
        .bearer_auth(&sender_token)
        .body(bytes.as_slice())
        .send()
        .await
        .expect("attachment chunk over sender Tailcat")
        .error_for_status()
        .expect("attachment chunk status");
    client
        .post(format!(
            "{sender_endpoint}/attachment/real-upload/commit?targetDevice={device_a}"
        ))
        .bearer_auth(&sender_token)
        .send()
        .await
        .expect("attachment commit over sender Tailcat")
        .error_for_status()
        .expect("attachment commit status");

    drop(reopened_chat);
    runtime_b.shutdown().await;
    drop(runtime_b);

    auth_b.shutdown_peer_preserving_session().await;
    drop(auth_b);
    let unavailable_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client
            .get(format!("{sender_endpoint}/pair/challenge"))
            .send()
            .await
            .is_err()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < unavailable_deadline,
            "sender adapter/listener remained reachable after EngineRuntime and Auth shutdown"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The target fetch uses the target's own endpoint and bearer while the
    // sender is provably unavailable. The bytes therefore come from durable
    // target custody, not from a still-running sender proxy.
    let target_token = auth_a.access_token().await.expect("target token");
    let fetched = client
        .get(format!(
            "{target_endpoint}/attachment/real-upload?senderDevice={device_b}&targetDevice={device_a}"
        ))
        .bearer_auth(target_token)
        .send()
        .await
        .expect("target-authenticated attachment fetch")
        .error_for_status()
        .expect("target attachment fetch status")
        .bytes()
        .await
        .expect("target attachment bytes");
    assert_eq!(fetched.as_ref(), bytes);
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(&fetched)),
        expected_digest
    );

    // Restore the sender from the same durable identity for the live
    // revocation phase below; it was completely absent during custody fetch.
    let auth_b = Auth::open(AuthConfig {
        data_dir: b_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .expect("reopen stopped sender auth");
    assert_eq!(auth_b.device_id().as_deref(), Some(device_b.as_str()));
    auth_b.resume().await;

    let endpoint = auth_b.peer_endpoint().expect("restored sender endpoint");
    let token = auth_b.access_token().await.expect("restored sender token");

    // A fresh viewer runtime supplies a live authenticated chat socket for the
    // revocation assertion below; attachment custody above used the stopped one.
    let runtime_b = Engine::assemble_runtime(
        &EngineConfig {
            data_dir: b_dir.path().to_owned(),
            ipc_port: 0,
            default_harness: kratos_proto::HarnessId::Mock,
        },
        auth_b.clone(),
        EngineProfile::synced(
            b_dir.path(),
            &paired.profile_id.clone().expect("profile"),
            "user-b",
        ),
    )
    .await
    .expect("live viewer runtime");
    let live_chat = runtime_b
        .core()
        .doc_host
        .open("real-chat")
        .expect("live revoked chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !live_chat.connected() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "live chat did not reconnect"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A separately initialized profile's token must not authenticate through
    // the A↔B Tailcat route.
    let c_dir = tempfile::tempdir().unwrap();
    let auth_c = Auth::open(AuthConfig {
        data_dir: c_dir.path().to_owned(),
        adapter_path: Some(adapter_bin.clone()),
        backup_interval: Duration::from_secs(60),
    })
    .unwrap();
    auth_c
        .initialize_peer("wrong-profile", Some(map_url.clone()))
        .await
        .expect("initialize wrong profile");
    let wrong_token = auth_c.access_token().await.expect("wrong profile token");
    let wrong_profile = client
        .get(format!("{endpoint}/registry/org/ws"))
        .bearer_auth(wrong_token)
        .send()
        .await
        .expect("wrong profile probe");
    assert_eq!(wrong_profile.status(), reqwest::StatusCode::UNAUTHORIZED);
    auth_c.sign_out();

    // Revocation is checked against the live authenticated peer route, and a
    // different profile is denied independently by the authenticated store.
    auth_a
        .revoke(&device_b)
        .await
        .expect("revoke paired device");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while live_chat.connected() {
        if tokio::time::Instant::now() >= deadline {
            let owner_token = auth_a.access_token().await.expect("owner token");
            let stats = client
                .get(format!(
                    "{}/chat2/real-chat/stats",
                    auth_a.peer_endpoint().expect("owner endpoint")
                ))
                .bearer_auth(owner_token)
                .send()
                .await
                .expect("revocation stats")
                .text()
                .await
                .expect("revocation stats body");
            panic!("revocation did not close the live chat socket; stats={stats}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let owner_token = auth_a.access_token().await.expect("owner token");
    let stats: serde_json::Value = client
        .get(format!(
            "{}/chat2/real-chat/stats",
            auth_a.peer_endpoint().expect("owner endpoint")
        ))
        .bearer_auth(owner_token)
        .send()
        .await
        .expect("post-revocation stats")
        .json()
        .await
        .expect("post-revocation stats body");
    assert_eq!(
        stats["connectedSockets"], 1,
        "only the owner's chat socket may remain after viewer revocation"
    );
    let revoked = client
        .get(format!("{endpoint}/registry/org/ws"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("revoked peer probe");
    assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);

    drop(live_chat);
    runtime_b.shutdown().await;
    runtime_a.shutdown().await;
    auth_a.sign_out();
    auth_b.sign_out();
    drop(fixture);
}
