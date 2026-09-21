//! Integrated regressions for local-first profile privacy and lifecycle boundaries.

use std::path::Path;
use std::sync::{Arc, Barrier};
use kratos_engine::{
    AuthState, Engine, EngineConfig, EngineCore, EngineProfile, HarnessId, WorkspaceScope,
    default_registry,
};

fn config(data_dir: &Path) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
    }
}

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

async fn shutdown(core: EngineCore) {
    core.shutdown().await;
    drop(core);
}

fn concurrent_engine_info(
    config: Arc<EngineConfig>,
    workers: usize,
) -> std::collections::HashSet<String> {
    let barrier = Arc::new(Barrier::new(workers));
    (0..workers)
        .map(|_| {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Engine::engine_info(&config, WorkspaceScope::Local)
                    .expect("resolve concurrent engine info")
                    .device_id
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|call| call.join().expect("engine-info worker"))
        .collect()
}

#[tokio::test]
async fn concurrent_engine_info_and_runtime_share_one_device_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Arc::new(config(dir.path()));
    let announced = concurrent_engine_info(config, 32);

    assert_eq!(announced.len(), 1, "every viewport announces one identity");
    let announced = announced.into_iter().next().expect("announced id");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("persisted device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(
        core.device_id, announced,
        "the assembled runtime must use the identity already announced"
    );
    shutdown(core).await;
}

#[tokio::test]
async fn empty_legacy_device_identity_is_repaired_once_for_all_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("device-id"), b"").expect("seed truncated identity");
    let config = Arc::new(config(dir.path()));

    let announced = concurrent_engine_info(config, 32);
    assert_eq!(
        announced.len(),
        1,
        "legacy repair must publish one identity"
    );
    let announced = announced.into_iter().next().expect("repaired id");
    assert!(!announced.trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("device-id"))
            .expect("repaired device id")
            .trim(),
        announced
    );

    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    assert_eq!(core.device_id, announced);
    shutdown(core).await;
}

#[tokio::test]
async fn local_and_synced_profiles_remain_isolated_across_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local_profile = EngineProfile::local(dir.path()).expect("local profile");
    let local_user_id = local_profile.user_id().to_string();
    let local_profile_file = dir.path().join("local-profile.json");
    let local_profile_bytes = std::fs::read(&local_profile_file).expect("local profile file");

    let local_upload = {
        let core = assemble(local_profile.clone());
        let device_id = core.device_id.clone();
        core.workspace
            .create_space(
                "local-space",
                &device_id,
                "/private/local-project",
                Some("Private project".into()),
                false,
            )
            .expect("create local space");
        core.workspace
            .create_chat("local-chat", Some("local-space"), None, None, None)
            .expect("create local chat");
        core.workspace
            .rename_chat("local-chat", "Private local session")
            .expect("name local chat");
        core.doc_host
            .open("local-chat")
            .expect("open local chat doc")
            .write_user_message("local-message", "Private local transcript", 1)
            .expect("write local transcript");
        core.uploads
            .append("local-upload", "cHJpdmF0ZQ==", Some(0))
            .expect("stage local upload");
        let upload = core
            .uploads
            .commit("local-upload", "private.png")
            .expect("commit local upload");
        assert!(Path::new(&upload).starts_with(local_profile.uploads_root()));
        shutdown(core).await;
        (device_id, upload)
    };

    let synced_profile = EngineProfile::synced(dir.path(), "cloud-org", "cloud-user");
    {
        let core = assemble(synced_profile.clone());
        assert_eq!(core.device_id, local_upload.0, "device identity is global");
        assert!(
            core.workspace
                .chat("local-chat")
                .expect("read synced chats")
                .is_none(),
            "the synced profile must not expose local chats"
        );
        assert_eq!(core.uploads.dir(), synced_profile.uploads_root());
        let error = core
            .uploads
            .read_chunk(&local_upload.1, 0, &[])
            .expect_err("synced upload jail must reject a local-profile path");
        assert!(
            error.to_string().contains("outside the upload cache"),
            "unexpected jail error: {error}"
        );

        core.workspace
            .create_space(
                "synced-space",
                &core.device_id,
                "/shared/cloud-project",
                None,
                false,
            )
            .expect("create synced space");
        core.workspace
            .create_chat("synced-chat", Some("synced-space"), None, None, None)
            .expect("create synced chat");
        shutdown(core).await;
    }

    let reopened_profile = EngineProfile::local(dir.path()).expect("reopen local profile");
    assert_eq!(reopened_profile.user_id(), local_user_id);
    assert_eq!(
        std::fs::read(&local_profile_file).expect("re-read local profile"),
        local_profile_bytes,
        "reopening local must not rotate or rewrite its identity"
    );
    {
        let core = assemble(reopened_profile);
        assert_eq!(core.device_id, local_upload.0);
        let chat = core
            .workspace
            .chat("local-chat")
            .expect("read local chat")
            .expect("local chat survived restart");
        assert_eq!(chat.title.as_deref(), Some("Private local session"));
        assert_eq!(chat.cwd.as_deref(), Some("/private/local-project"));
        let transcript = core
            .doc_host
            .open("local-chat")
            .expect("reopen local chat doc")
            .doc()
            .read_entries()
            .expect("read local transcript");
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].id, "local-message");
        assert!(
            core.workspace
                .chat("synced-chat")
                .expect("read local chats")
                .is_none(),
            "the local profile must not expose synced chats"
        );
        assert_eq!(
            core.uploads
                .read_chunk(&local_upload.1, 0, &[])
                .expect("local profile can read its upload")
                .data,
            "cHJpdmF0ZQ=="
        );
        shutdown(core).await;
    }
}

#[tokio::test]
async fn fresh_paired_profile_ignores_every_historical_storage_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local_profile = EngineProfile::local(dir.path()).expect("local profile");
    std::fs::create_dir_all(local_profile.uploads_root()).expect("local uploads");
    let local_upload = local_profile.uploads_root().join("local.png");
    std::fs::write(&local_upload, b"local").expect("local upload");

    let global_upload = dir.path().join("uploads/global.png");
    std::fs::create_dir_all(global_upload.parent().unwrap()).expect("global uploads");
    std::fs::write(&global_upload, b"global").expect("global upload");
    std::fs::write(
        dir.path().join("legacy-uploads-owner.json"),
        br#"{"orgId":"old-org","userId":"old-user"}"#,
    )
    .expect("obsolete owner metadata");

    let old_account = EngineProfile::synced(dir.path(), "old-org", "old-user");
    std::fs::create_dir_all(old_account.uploads_root()).expect("old account uploads");
    let old_upload = old_account.uploads_root().join("old.png");
    std::fs::write(&old_upload, b"old account").expect("old account upload");

    let profile_id = uuid::Uuid::new_v4().to_string();
    let paired = EngineProfile::paired(dir.path(), &profile_id).expect("paired profile");
    let core = assemble(paired.clone());
    assert_eq!(core.uploads.dir(), paired.uploads_root());
    for path in [&local_upload, &global_upload, &old_upload] {
        let error = core
            .uploads
            .read_chunk(path.to_str().unwrap(), 0, &[])
            .expect_err("fresh paired profile must reject historical attachment roots");
        assert!(error.to_string().contains("outside the upload cache"));
    }
    assert!(core.workspace.read_chats().unwrap().is_empty());
    shutdown(core).await;

    assert!(local_upload.is_file(), "local data must not be deleted");
    assert!(global_upload.is_file(), "global data must not be deleted");
    assert!(old_upload.is_file(), "old account data must not be deleted");
}

#[tokio::test]
async fn legacy_workos_session_is_never_an_implicit_profile_authority() {
    let dir = tempfile::tempdir().expect("tempdir");
    let historical = EngineProfile::synced(dir.path(), "legacy-org", "legacy-user");
    {
        let core = assemble(historical.clone());
        core.workspace
            .create_space(
                "legacy-space",
                &core.device_id,
                "/shared/legacy-project",
                None,
                false,
            )
            .expect("create historical space");
        core.workspace
            .create_chat("legacy-chat", Some("legacy-space"), None, None, None)
            .expect("create historical chat");
        shutdown(core).await;
    }
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"refresh-1","user":{"id":"legacy-user","email":"legacy@example.com"},"orgId":"legacy-org"}"#,
    )
    .expect("seed untrusted WorkOS session");

    let config = config(dir.path());
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let resolved = Engine::resolve_profile(&config, &auth, scope)
        .expect("resolve profile")
        .expect("local profile is ready");

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(scope, WorkspaceScope::Local);
    assert_eq!(resolved.scope(), WorkspaceScope::Local);
    assert_ne!(resolved.store_root(), historical.store_root());
    let core = assemble(resolved);
    assert!(
        core.workspace
            .chat("legacy-chat")
            .expect("read local registry")
            .is_none(),
        "an old session.json must not expose historical account data"
    );
    shutdown(core).await;
    assert!(
        dir.path().join("session.json").is_file(),
        "opening auth must not mutate migration source material"
    );
}
#[tokio::test]
async fn removed_local_import_rpc_names_are_unknown() {
    use kratos_rpc::{RpcError, RpcService};

    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    let rpc = core.rpc_service();
    for method in ["ImportLocalWorkspace", "LocalImportStatus"] {
        assert!(matches!(
            rpc.handle(method, serde_json::json!({})).await,
            Err(RpcError::UnknownMethod(name)) if name == method
        ));
    }
    shutdown(core).await;
}

#[tokio::test]
async fn opening_peer_auth_does_not_activate_sync_for_the_running_local_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());
    let auth = Engine::build_auth(&config).await.expect("open auth");
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .expect("resolve local profile")
        .expect("local profile is ready");
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .expect("assemble local runtime");

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(scope, WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());

    runtime
        .core()
        .workspace
        .create_space(
            "still-local-space",
            &runtime.core().device_id,
            "/private/still-local",
            None,
            false,
        )
        .expect("create space");
    runtime
        .core()
        .workspace
        .create_chat(
            "still-local-chat",
            Some("still-local-space"),
            None,
            None,
            None,
        )
        .expect("create chat");
    runtime
        .core()
        .doc_host
        .open("still-local-chat")
        .expect("open local chat doc")
        .write_user_message("message-1", "This stays local", 1)
        .expect("write local message");
    runtime
        .core()
        .uploads
        .append("still-local-upload", "cHJpdmF0ZQ==", Some(0))
        .expect("stage local upload");
    let upload = runtime
        .core()
        .uploads
        .commit("still-local-upload", "private.png")
        .expect("commit local upload");

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(Path::new(&upload).starts_with(dir.path().join("profiles/local/uploads")));
    runtime.shutdown().await;
}
