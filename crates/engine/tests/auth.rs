//! Peer-session Auth and managed adapter integration tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kratos_engine::{
    Auth, AuthConfig, AuthState, Engine, EngineConfig, HarnessId, PairingInvitation, WorkspaceScope,
};

use kratos_engine::peer_runtime::backup::verify_generation;

fn auth_config(data_dir: &Path, adapter: &Path) -> AuthConfig {
    let mut config = AuthConfig::new(data_dir);
    config.adapter_path = Some(adapter.to_path_buf());
    config.backup_interval = Duration::from_millis(50);
    config
}

fn engine_config(data_dir: &Path) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
    }
}

#[test]
fn invitation_wire_is_versioned_compact_and_rejects_untrusted_derp_urls() {
    let invitation = PairingInvitation {
        version: 1,
        address: format!("tc{}", "a".repeat(80)),
        invite: kratos_engine::peer_auth::Invite {
            version: 1,
            profile_id: uuid::Uuid::new_v4().to_string(),
            invite_id: uuid::Uuid::new_v4().to_string(),
            secret: "A".repeat(43),
            expires_at: i64::MAX,
        },
        derp_map: Some("https://derp.example/map.json".into()),
    };
    let encoded = invitation.encode().expect("encode invitation");
    assert!(encoded.starts_with("kratos-pair:"));
    let parsed = PairingInvitation::parse(&encoded).expect("parse compact invitation");
    assert_eq!(parsed.address, invitation.address);
    assert_eq!(parsed.invite.profile_id, invitation.invite.profile_id);
    let raw = serde_json::to_string(&invitation).expect("raw invitation");
    assert_eq!(
        PairingInvitation::parse(&raw)
            .expect("parse raw JSON")
            .address,
        invitation.address
    );

    let mut insecure = invitation;
    insecure.derp_map = Some("http://derp.example/map.json".into());
    assert!(insecure.encode().is_err());
    assert!(PairingInvitation::parse("kratos-pair:not-base64").is_err());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    use base64::Engine as _;
    use kratos_engine::peer_auth::{DeviceIdentity, RedeemRequest};

    struct Fixture {
        _root: tempfile::TempDir,
        host_dir: PathBuf,
        client_dir: PathBuf,
        target: PathBuf,

        adapter: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("fixture root");
            let map = root.path().join("target");
            let adapter = root.path().join("kratos-tailcat");
            let script = format!(
                r#"#!/bin/sh
set -eu
mode="$1"
shift
target_file='{}'
case "$mode" in
  serve)
    target=""
    state=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --target) target="$2"; shift 2 ;;
        --state) state="$2"; shift 2 ;;
        --derp-map|--region) shift 2 ;;
        *) exit 64 ;;
      esac
    done
    [ -n "$target" ]
    [ -n "$state" ]
    printf '%s' "$$" > "$state.pid"

    [ -n "$state" ]
    printf '%s' 'stable-tailcat-server-key' > "$state"
    chmod 600 "$state"
    printf '%s' "$target" > "$target_file"
    printf '%s\n' '{{"address":"tcaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}'
    ;;
  connect)
    config=""
    listen=""
    state=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --config) config="$2"; shift 2 ;;
        --listen) listen="$2"; shift 2 ;;
        --state) state="$2"; shift 2 ;;
        --derp-map) shift 2 ;;
        *) exit 64 ;;
      esac
    done
    [ -f "$config" ]
    [ -n "$state" ]
    printf '%s' "$$" > "$state.pid"

    printf '%s' "$listen" > "$state.listen"
    [ "$(LC_ALL=C ls -ld "$config" | cut -c 1-10)" = '-rw-------' ]
    grep -q '"address":"tc' "$config"
    target="$(cat "$target_file")"
    printf '{{"url":"http://%s"}}\n' "$target"
    ;;
  *) exit 64 ;;
esac
exec sleep 86400 >/dev/null 2>&1
"#,
                map.display()
            );
            std::fs::write(&adapter, script).expect("write adapter");
            std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700))
                .expect("adapter mode");
            Self {
                host_dir: root.path().join("host"),
                client_dir: root.path().join("client"),
                target: map,

                adapter,
                _root: root,
            }
        }
    }

    async fn reject_repair_redeem(
        axum::extract::State(requests): axum::extract::State<
            std::sync::Arc<std::sync::Mutex<Vec<RedeemRequest>>>,
        >,
        axum::Json(request): axum::Json<RedeemRequest>,
    ) -> axum::http::StatusCode {
        let mut requests = requests.lock().expect("redeem request lock");
        let status = if requests.is_empty() {
            axum::http::StatusCode::FORBIDDEN
        } else {
            axum::http::StatusCode::UNAUTHORIZED
        };
        requests.push(request);
        status
    }

    #[tokio::test]
    async fn managed_host_pair_runtime_restart_and_revocation_use_real_http_proofs() {
        // Engine assembly warms the user's interactive login-shell PATH. This
        // test has no CLI discovery work and runs under a detached test PTY,
        // where an interactive shell would correctly stop on SIGTTIN.
        unsafe { std::env::set_var("KRATOS_NO_LOGIN_SHELL", "1") };
        let fixture = Fixture::new();
        let host = Auth::open(auth_config(&fixture.host_dir, &fixture.adapter)).expect("host auth");
        assert_eq!(host.state(), AuthState::SignedOut);
        let status = host
            .initialize_peer("host", None)
            .await
            .expect("initialize host");
        assert!(status.hosting && status.connected && status.signed_in);
        let host_profile = status.profile_id.clone().expect("host profile");
        assert_eq!(host_profile, host.state().user().expect("auth user").id);

        let code = host.invite().await.expect("create invitation");
        assert!(
            !code.contains("tcaaaaaaaa"),
            "compact code must not expose plaintext address"
        );
        let client =
            Auth::open(auth_config(&fixture.client_dir, &fixture.adapter)).expect("client auth");
        client.pair(&code).await.expect("pair client");
        let client_status = client.peer_status().await;
        assert!(client_status.signed_in && client_status.connected && !client_status.hosting);
        assert_eq!(
            client_status.profile_id.as_deref(),
            Some(host_profile.as_str())
        );
        let authenticated_device = client_status.device_id.clone().expect("paired device");
        assert!(client.access_token().await.is_some());

        let devices = host.devices().await.expect("owner device list");
        assert_eq!(devices.len(), 2);
        assert!(
            devices
                .iter()
                .any(|device| device.device_id == authenticated_device)
        );
        assert!(
            client.devices().await.is_err(),
            "non-owner cannot manage devices"
        );

        let stable_url = client.peer_url().expect("client adapter URL");
        let config = engine_config(&fixture.client_dir);
        let scope = Engine::initial_workspace_scope(&client);
        assert_eq!(scope, WorkspaceScope::Synced);
        let profile = Engine::resolve_profile(&config, &client, scope)
            .expect("resolve paired profile")
            .expect("paired profile");
        assert_eq!(
            profile.store_root(),
            fixture.client_dir.join("profiles").join(&host_profile)
        );
        let runtime = Engine::assemble_runtime(&config, client.clone(), profile)
            .await
            .expect("assemble paired runtime");
        assert_eq!(runtime.core().device_id, authenticated_device);
        let historical = std::fs::read_to_string(fixture.client_dir.join("device-id"))
            .expect("historical device id");
        assert_ne!(historical.trim(), runtime.core().device_id);
        // Retaining an Auth/RPC clone must not retain the child after Quit.
        let retained_rpc = runtime.core().rpc_service();
        runtime.shutdown().await;
        assert!(
            !client.peer_status().await.connected,
            "shutdown retained the adapter"
        );
        assert!(
            client.state().is_signed_in(),
            "shutdown must preserve pairing"
        );
        assert!(
            client.access_token().await.is_none(),
            "late consumers restarted a stopped adapter"
        );
        client.resume().await;
        assert!(
            !client.peer_status().await.connected,
            "late startup resumed a stopped adapter"
        );
        drop(retained_rpc);
        drop(runtime);

        drop(client);
        let reopened = Auth::open(auth_config(&fixture.client_dir, &fixture.adapter))
            .expect("reopen client auth");
        assert!(reopened.state().is_signed_in());
        reopened.resume().await;
        assert_eq!(reopened.peer_url().as_deref(), Some(stable_url.as_str()));
        assert!(reopened.access_token().await.is_some());

        let listen_file = fixture.client_dir.join("peer/tailcat-client.key.listen");
        let stable_listen = std::fs::read_to_string(&listen_file).expect("initial client listen");

        // A valid cached bearer must not mask a crashed adapter. The next
        // authenticated operation restarts it on the persisted loopback port.
        let endpoint = reopened.peer_endpoint().expect("stable peer endpoint");
        let adapter_pid: i32 =
            std::fs::read_to_string(fixture.client_dir.join("peer/tailcat-client.key.pid"))
                .expect("client adapter pid")
                .parse()
                .expect("numeric client adapter pid");
        unsafe { libc::kill(adapter_pid, libc::SIGKILL) };
        tokio::time::timeout(Duration::from_secs(2), async {
            while reopened.peer_status().await.connected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("observe adapter exit");
        assert!(reopened.access_token().await.is_some());
        assert_eq!(reopened.peer_url().as_deref(), Some(stable_url.as_str()));
        assert_eq!(reopened.peer_endpoint().as_deref(), Some(endpoint.as_str()));
        assert_eq!(
            std::fs::read_to_string(&listen_file).expect("restarted client listen"),
            stable_listen
        );

        host.revoke(&authenticated_device)
            .await
            .expect("revoke paired device");
        reopened.sign_out();
        assert_eq!(reopened.state(), AuthState::SignedOut);
        assert!(!fixture.client_dir.join("peer-session.json").exists());

        // Sign-out retains the installation's signing credential. A revoked
        // credential cannot be revived, so normal Pair must stage a fresh key
        // and publish it only after redemption succeeds.
        let retained_identity = std::fs::read(fixture.client_dir.join("peer-device.json"))
            .expect("retained signing identity");
        assert!(reopened.pair("not-an-invitation").await.is_err());
        assert_eq!(
            std::fs::read(fixture.client_dir.join("peer-device.json")).unwrap(),
            retained_identity,
            "a failed pair must preserve the recoverable existing credential"
        );
        let repair_code = host.invite().await.expect("fresh repair invitation");
        reopened
            .pair(&repair_code)
            .await
            .expect("re-pair revoked installation through normal API");
        let repaired_device = reopened.device_id().expect("repaired device id");
        assert_ne!(repaired_device, authenticated_device);
        assert!(reopened.access_token().await.is_some());
        let devices = host.devices().await.expect("device list after repair");
        assert!(devices.iter().any(|device| {
            device.device_id == authenticated_device && device.revoked_at.is_some()
        }));
        assert!(
            devices.iter().any(|device| {
                device.device_id == repaired_device && device.revoked_at.is_none()
            })
        );

        let backup_root = fixture.host_dir.join("peer/backups");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backup_root.join("latest.json").is_file() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("scheduled complete peer backup");
        let latest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(backup_root.join("latest.json")).unwrap())
                .unwrap();
        let generation = backup_root.join(latest["generationId"].as_str().unwrap());
        verify_generation(&generation).expect("published generation verifies");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_repair_publishes_exactly_one_consistent_identity_and_session() {
        let fixture = Fixture::new();
        let host = Auth::open(auth_config(&fixture.host_dir, &fixture.adapter)).expect("host auth");
        host.initialize_peer("host", None)
            .await
            .expect("initialize host");
        let client =
            Auth::open(auth_config(&fixture.client_dir, &fixture.adapter)).expect("client auth");
        client
            .pair(&host.invite().await.expect("initial invitation"))
            .await
            .expect("initial pair");
        let revoked_device = client.device_id().expect("initial device");
        host.revoke(&revoked_device).await.expect("revoke client");
        client.sign_out();

        let codes = (
            host.invite().await.expect("first repair invitation"),
            host.invite().await.expect("second repair invitation"),
        );
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let first = {
            let client = client.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                client.pair(&codes.0).await
            })
        };
        let second = {
            let client = client.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                client.pair(&codes.1).await
            })
        };
        barrier.wait().await;
        let results = (
            first.await.expect("first task"),
            second.await.expect("second task"),
        );
        assert_eq!(
            usize::from(results.0.is_ok()) + usize::from(results.1.is_ok()),
            1,
            "only one concurrent profile transition may publish"
        );

        let repaired_device = client.device_id().expect("winning repair device");
        assert_ne!(repaired_device, revoked_device);
        let persisted_identity =
            std::fs::read(fixture.client_dir.join("peer-device.json")).expect("persisted identity");
        drop(client);

        let reopened = Auth::open(auth_config(&fixture.client_dir, &fixture.adapter))
            .expect("reopen repaired client");
        assert_eq!(
            reopened.device_id().as_deref(),
            Some(repaired_device.as_str())
        );
        assert_eq!(
            std::fs::read(fixture.client_dir.join("peer-device.json")).unwrap(),
            persisted_identity
        );
        reopened.resume().await;
        assert!(
            reopened.access_token().await.is_some(),
            "the persisted session must authenticate with the persisted key"
        );
    }

    #[tokio::test]
    async fn rejected_fresh_repair_candidate_preserves_retained_identity() {
        let fixture = Fixture::new();
        let host = Auth::open(auth_config(&fixture.host_dir, &fixture.adapter)).expect("host auth");
        host.initialize_peer("host", None)
            .await
            .expect("initialize host");
        let client =
            Auth::open(auth_config(&fixture.client_dir, &fixture.adapter)).expect("client auth");
        client
            .pair(&host.invite().await.expect("initial invitation"))
            .await
            .expect("initial pair");
        let revoked_device = client.device_id().expect("initial device");
        host.revoke(&revoked_device).await.expect("revoke client");
        client.sign_out();
        let identity_path = fixture.client_dir.join("peer-device.json");
        let retained_identity = std::fs::read(&identity_path).expect("retained identity");
        let retained_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            DeviceIdentity::load_or_create(&identity_path)
                .expect("load retained identity")
                .public_key(),
        );
        let repair_code = host.invite().await.expect("repair invitation");

        // Redirect this connection to a controlled peer: the retained key gets
        // the 403 that activates fallback, then the fresh candidate is rejected.
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/pair/redeem", axum::routing::post(reject_repair_redeem))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled peer");
        std::fs::write(
            &fixture.target,
            listener
                .local_addr()
                .expect("controlled peer address")
                .to_string(),
        )
        .expect("redirect adapter");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve controlled peer");
        });

        assert!(client.pair(&repair_code).await.is_err());
        server.abort();
        let requests = requests.lock().expect("redeem requests");
        assert_eq!(requests.len(), 2, "fallback must make exactly two redeems");
        assert_eq!(
            requests[0].public_key, retained_public_key,
            "the first redeem must use the retained key"
        );
        assert_ne!(
            requests[1].public_key, retained_public_key,
            "the second redeem must use a fresh candidate key"
        );
        assert_ne!(requests[1].public_key, requests[0].public_key);
        drop(requests);

        assert_eq!(client.state(), AuthState::SignedOut);
        assert!(!fixture.client_dir.join("peer-session.json").exists());
        assert_eq!(
            std::fs::read(identity_path).unwrap(),
            retained_identity,
            "a rejected fresh candidate must not replace the retained key"
        );
    }

    #[tokio::test]
    async fn failed_host_readiness_releases_the_reserved_listener_port() {
        use kratos_engine::peer_auth::AuthStore;
        use kratos_engine::peer_runtime::{PeerRuntime, PeerRuntimeConfig};

        let fixture = Fixture::new();
        let bad = fixture._root.path().join("bad-readiness-tailcat");
        std::fs::write(&bad, "#!/bin/sh\nprintf 'not-json\\n'\nexec sleep 86400\n")
            .expect("bad adapter");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).expect("bad mode");
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve probe");
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let data_dir = fixture._root.path().join("startup-retry");
        std::fs::create_dir_all(data_dir.join("peer")).expect("peer directory");

        let store = AuthStore::open(data_dir.join("peer/auth.sqlite")).expect("auth store");
        let mut config = PeerRuntimeConfig::new(&data_dir);
        config.local_port = port;
        config.adapter_path = Some(bad);
        assert!(PeerRuntime::host(&config, store.clone()).await.is_err());

        config.adapter_path = Some(fixture.adapter.clone());
        let runtime = PeerRuntime::host(&config, store)
            .await
            .expect("same-port retry after readiness failure");
        assert_eq!(runtime.local_url(), format!("http://127.0.0.1:{port}"));
    }

    #[tokio::test]
    async fn offline_resume_preserves_signed_in_profile_and_reports_error() {
        let fixture = Fixture::new();
        let host = Auth::open(auth_config(&fixture.host_dir, &fixture.adapter)).expect("host auth");
        host.initialize_peer("host", None)
            .await
            .expect("initialize");
        let code = host.invite().await.expect("invite");
        let client =
            Auth::open(auth_config(&fixture.client_dir, &fixture.adapter)).expect("client");
        client.pair(&code).await.expect("pair");
        let profile = client.profile_id().expect("profile");
        client.sign_out();

        // A malformed readiness process fails visibly and never creates an
        // authenticated profile as a side effect.
        let bad = fixture._root.path().join("bad-tailcat");
        std::fs::write(&bad, "#!/bin/sh\nprintf 'not-json\\n'\n").expect("bad adapter");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).expect("bad mode");
        let isolated = fixture._root.path().join("isolated");
        let auth = Auth::open(auth_config(&isolated, &bad)).expect("isolated auth");
        assert!(auth.initialize_peer("host", None).await.is_err());
        assert_eq!(auth.state(), AuthState::SignedOut);
        assert_ne!(auth.profile_id().as_deref(), Some(profile.as_str()));
    }
}
