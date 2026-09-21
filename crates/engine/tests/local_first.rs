//! Local-first startup boundaries and captured synced-session behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};
use kratos_engine::{AuthState, Engine, EngineConfig, EngineInfo, HarnessId, WorkspaceScope};
use kratos_rpc::{connect_ws, memory_client, methods};

fn config(
    data_dir: &std::path::Path,
    _edge_url: String,
    _workos_client_id: Option<&str>,
    _edge_token: Option<&str>,
) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
    }
}

async fn rejecting_edge() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                seen.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"revoked"}"#;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), requests, task)
}

struct DaemonEdge {
    url: String,
    active: Arc<Mutex<HashMap<String, usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl DaemonEdge {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let active = Arc::new(Mutex::new(HashMap::new()));
        let active_for_task = active.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let active = active_for_task.clone();
                tokio::spawn(async move { serve_daemon_edge(stream, active).await });
            }
        });
        Self { url, active, task }
    }

    fn active_matching(&self, prefix: &str) -> usize {
        self.active
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(_, count)| *count)
            .sum()
    }

    fn active_total(&self) -> usize {
        self.active.lock().unwrap().values().sum()
    }
}

impl Drop for DaemonEdge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ActiveSocket {
    active: Arc<Mutex<HashMap<String, usize>>>,
    path: String,
}

impl Drop for ActiveSocket {
    fn drop(&mut self) {
        let mut active = self.active.lock().unwrap();
        if let Some(count) = active.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.path);
            }
        }
    }
}

async fn serve_daemon_edge(
    mut stream: tokio::net::TcpStream,
    active: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut peeked = [0u8; 8192];
    let request_len = loop {
        let Ok(read) = stream.peek(&mut peeked).await else {
            return;
        };
        if read == 0 {
            return;
        }
        if peeked[..read].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break read;
        }
        tokio::task::yield_now().await;
    };
    let headers = String::from_utf8_lossy(&peeked[..request_len]);
    if !headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("upgrade: websocket"))
    {
        let mut request = [0u8; 8192];
        let Ok(read) = stream.read(&mut request).await else {
            return;
        };
        let request = String::from_utf8_lossy(&request[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let body = if target.starts_with("/auth/refresh") {
            let claims = serde_json::json!({
                "exp": 4_102_444_800_u64,
                "org_id": "org_1"
            });
            let claims =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            serde_json::json!({
                "accessToken": format!("e30.{claims}.sig"),
                "refreshToken": "rotated-refresh"
            })
            .to_string()
        } else {
            r#"{"releases":[]}"#.to_string()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    let path = Arc::new(Mutex::new(String::new()));
    let captured = path.clone();
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &WsRequest, response: WsResponse| {
            *captured.lock().unwrap() = request.uri().path().to_string();
            Ok(response)
        },
    )
    .await
    else {
        return;
    };
    let path = path.lock().unwrap().clone();
    *active.lock().unwrap().entry(path.clone()).or_default() += 1;
    let _active = ActiveSocket {
        active,
        path: path.clone(),
    };
    let (mut sink, mut source) = ws.split();
    while let Some(frame) = source.next().await {
        let Ok(frame) = frame else { return };
        if path.starts_with("/registry/")
            && let WsMessage::Text(text) = frame
        {
            if text == "ping" {
                if sink.send(WsMessage::Text("pong".into())).await.is_err() {
                    return;
                }
            } else if serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|frame| {
                    frame
                        .get("t")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .as_deref()
                == Some("hello")
            {
                let state = serde_json::json!({
                    "t": "state",
                    "seq": 0,
                    "full": true,
                    "gcFloor": 0,
                    "rows": [],
                    "presence": {}
                });
                if sink
                    .send(WsMessage::Text(state.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn wait_until(mut check: impl FnMut() -> bool, message: &str) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{message}");
}

#[tokio::test]
async fn signed_out_workos_boot_serves_local_data_without_dev_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready without auth");

    assert_eq!(scope, WorkspaceScope::Local);
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    let client = memory_client(runtime.core().rpc_service());
    let info: EngineInfo = client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(info.workspace_scope, WorkspaceScope::Local);
    assert!(
        client
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap()
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(dir.path().join("profiles/local").is_dir());
    assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
    assert!(runtime.core().links().is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn clean_local_auth_construction_does_not_probe_edge_health() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);

    let auth = Engine::build_auth(&config).await.expect("open auth");

    assert_eq!(
        Engine::initial_workspace_scope(&auth),
        WorkspaceScope::Local
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    edge_task.abort();
}

#[tokio::test]
async fn local_runtime_does_not_start_the_edge_updater() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(
        runtime.core().updater().is_none(),
        "local runtime must not start an Edge updater"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn unverified_legacy_session_is_ignored_and_local_profile_opens_offline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"dead","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile resolves");

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(scope, WorkspaceScope::Local);
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Local);
    assert!(dir.path().join("profiles/local").is_dir());
    assert!(!dir.path().join("orgs/org_1/user_1").exists());
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn legacy_refresh_session_cannot_activate_peer_supervisors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile resolves");
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(scope, WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(runtime.core().updater().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn signed_out_runtime_stays_local_without_a_peer_session() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, None, None);
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready");
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Local);
    assert_eq!(auth.access_token().await, None);
    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn legacy_dev_bearer_is_not_an_authentication_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        None,
        Some("dev-user@dev-org"),
    );
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready");
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
    runtime.shutdown().await;
}

#[tokio::test]
async fn headless_stop_rpc_drains_the_daemon_and_releases_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");

    assert_eq!(
        client
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .unwrap(),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop")
        .expect("headless task panicked")
        .expect("headless shutdown failed");

    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after shutdown");
}

#[tokio::test]
async fn headless_sign_out_from_local_mode_stops_daemon_and_releases_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("legacy-client"),
        None,
    );
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());
    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");

    assert_eq!(
        client
            .call(methods::SIGN_OUT, serde_json::json!({}))
            .await
            .expect("SignOut reply"),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop after sign-out")
        .expect("headless task panicked")
        .expect("headless shutdown failed");
    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after sign-out");
}

/// Replacing a synced/online runtime must be a real ownership boundary: after
/// `shutdown()` returns, no worker may send another Edge request (updater,
/// room joins), and dropping the runtime must actually free
/// the engine graph — the sessions ⇄ doc-host cycle and the strong-`self`
/// worker loops previously kept a replaced runtime alive and polling forever.
#[tokio::test]
async fn configured_edge_core_shutdown_stops_workers_and_retires_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let profile = kratos_engine::EngineProfile::local(dir.path()).expect("local profile");
    let core = kratos_engine::EngineCore::assemble_with_profile(
        profile,
        Arc::new(kratos_engine::default_registry()),
        HarnessId::Mock,
        Some(kratos_engine::EdgeConfig::with_static_token(
            edge_url,
            "test-token",
        )),
    )
    .expect("assemble edge core");
    let retired = core.doc_host.retirement_probe();
    wait_until(
        || requests.load(Ordering::SeqCst) >= 1,
        "edge worker never produced traffic before shutdown",
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(30), core.shutdown())
        .await
        .expect("shutdown never returned");
    let after = requests.load(Ordering::SeqCst);
    drop(core);
    wait_until(
        &*retired,
        "engine graph still reachable after shutdown + drop",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        after,
        "edge received requests after shutdown returned"
    );
    edge_task.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_profile_cold_boots_offline_then_flushes_cached_chat_and_queue() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use kratos_engine::peer_auth::{AuthStore, DeviceIdentity};
    use kratos_engine::{Auth, AuthConfig, EngineProfile, default_registry};

    // Detached test runners can stop interactive login shells on SIGTTIN.
    unsafe { std::env::set_var("KRATOS_NO_LOGIN_SHELL", "1") };

    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("device");
    std::fs::create_dir_all(data.join("peer")).expect("peer dir");
    let identity =
        DeviceIdentity::load_or_create(data.join("peer-device.json")).expect("device identity");
    let auth_store = AuthStore::open(data.join("peer/auth.sqlite")).expect("auth store");
    let principal = auth_store
        .create_profile(&identity.public_key(), Some("offline device"))
        .expect("profile");
    drop(auth_store);

    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve endpoint");
    let port = reserved.local_addr().expect("reserved address").port();
    drop(reserved);
    std::fs::write(
        data.join("peer-session.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "profileId": principal.profile_id,
            "deviceId": principal.device_id,
            "address": format!("tc{}", "a".repeat(80)),
            "hosting": true,
            "localPort": port
        }))
        .expect("session json"),
    )
    .expect("session file");

    let profile = EngineProfile::paired(&data, &principal.profile_id).expect("paired profile");
    {
        let seeded = kratos_engine::EngineCore::assemble_with_profile(
            profile.clone(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .expect("seed cached profile");
        seeded
            .workspace
            .create_chat("offline-chat", None, Some("remote-device"), None, None)
            .expect("seed chat");
        seeded
            .doc_host
            .open("offline-chat")
            .expect("cached chat")
            .write_user_message("cached-message", "available before reconnect", 1)
            .expect("cached transcript");
        seeded
            .workspace
            .set_chat_room_gen("offline-chat", 2)
            .expect("mark previously synced room");

        seeded.shutdown().await;
    }

    // Model a profile that completed chat2 adoption before going offline: its
    // cached snapshot is epoch 2 and remains immediately readable on restart.
    let docs = kratos_sync::DocsStore::open(profile.store_root()).expect("cached docs");
    let (snapshot, cursor, _) = docs
        .load_snapshot_with_cursor("offline-chat")
        .expect("load cached snapshot")
        .expect("cached snapshot exists");
    docs.save_snapshot_with_cursor("offline-chat", &snapshot, cursor, 2)
        .expect("stamp chat2 cache");

    let adapter = dir.path().join("kratos-tailcat");
    let mut auth_config = AuthConfig::new(&data);
    auth_config.adapter_path = Some(adapter.clone());
    let auth = Auth::open(auth_config).expect("open offline auth");
    assert_eq!(
        Engine::initial_workspace_scope(&auth),
        WorkspaceScope::Synced
    );
    let info = Engine::engine_info(
        &config(&data, String::new(), None, None),
        WorkspaceScope::Synced,
    )
    .expect("synced engine info");
    assert_eq!(info.device_id, principal.device_id);

    let started = Instant::now();
    let runtime = Engine::assemble_runtime(
        &config(&data, String::new(), None, None),
        auth.clone(),
        profile,
    )
    .await
    .expect("offline runtime");
    assert!(started.elapsed() < Duration::from_secs(2));

    let chat = runtime
        .core()
        .doc_host
        .open("offline-chat")
        .expect("reopen cached chat");
    assert!(
        chat.doc()
            .read_entries()
            .expect("cached entries")
            .iter()
            .any(|entry| entry.id == "cached-message")
    );
    let queued = runtime
        .core()
        .doc_host
        .queue_message_with_behavior("offline-chat", "send after reconnect", Vec::new(), true)
        .expect("durable offline queue");
    assert!(
        chat.doc()
            .read_queue()
            .expect("offline queue")
            .iter()
            .any(|item| item.id == queued)
    );

    let remote_db = data.join("peer/peer.sqlite");
    std::fs::write(
        &adapter,
        "#!/bin/sh\nset -eu\n[ \"$1\" = serve ]\nprintf '%s\\n' '{\"address\":\"tcaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}'\nexec sleep 86400 >/dev/null 2>&1\n",
    )
    .expect("recovered adapter");
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700))
        .expect("adapter mode");
    auth.resume().await;
    let token = auth.access_token().await.expect("reconnect authenticates");
    let stats_response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/chat2/offline-chat/stats"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("reconnected peer HTTP");
    assert!(stats_response.status().is_success());

    let registry_probe = format!(
        "ws://127.0.0.1:{port}/registry/{}/ws?token={token}&device={}",
        principal.profile_id, principal.device_id
    );
    let (socket, _) = tokio_tungstenite::connect_async(&registry_probe)
        .await
        .expect("protected registry websocket reconnects");
    drop(socket);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let caught_up = loop {
        let caught_up = rusqlite::Connection::open(&remote_db)
            .and_then(|db| {
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM chat_rows WHERE profile=?1 AND chat=?2) OR EXISTS(SELECT 1 FROM chat_checkpoints WHERE profile=?1 AND chat=?2)",
                    rusqlite::params![principal.profile_id, "offline-chat"],
                    |row| row.get::<_, bool>(0),
                )
            })
            .unwrap_or(false);
        if caught_up || tokio::time::Instant::now() >= deadline {
            break caught_up;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (registry_rows, chat_rows): (u64, u64) = rusqlite::Connection::open(&remote_db)
        .and_then(|db| {
            let registry =
                db.query_row("SELECT COUNT(*) FROM registry_rows", [], |row| row.get(0))?;
            let chats = db.query_row("SELECT COUNT(*) FROM chat_rows", [], |row| row.get(0))?;
            Ok((registry, chats))
        })
        .expect("inspect catch-up database");
    assert!(
        caught_up,
        "cached transcript and queue did not catch up: registry_rows={registry_rows}, chat_rows={chat_rows}"
    );
    assert!(
        chat.doc()
            .read_queue()
            .expect("queue after reconnect")
            .iter()
            .any(|item| item.id == queued),
        "a command for another host must remain durably queued"
    );

    runtime.shutdown().await;
}
