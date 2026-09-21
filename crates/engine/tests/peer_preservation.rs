//! Authenticated PeerStore + EngineCore preservation coverage.
//!
//! The test host is a real loopback Axum listener backed by `PeerStore`; its
//! middleware validates the same Ed25519 bearer tokens produced by `AuthStore`.
//! Tailcat is intentionally not started here: the runtime adapter owns that
//! separately, while this test isolates the application protocol carried over
//! the local endpoint.

#[path = "../src/peer_auth.rs"]
mod peer_auth;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tempfile::TempDir;
use kratos_engine::{EngineCore, HarnessRegistry};
use kratos_rpc::{RpcService, memory_client, methods};
use kratos_sync::peer::{PeerPrincipal, PeerStore, router};

use peer_auth::{AuthStore, DeviceIdentity, Principal};

async fn authenticated(
    State(store): State<AuthStore>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            request.uri().query().and_then(|query| {
                query.split('&').find_map(|part| {
                    let (key, value) = part.split_once('=')?;
                    (key == "token").then_some(value)
                })
            })
        });
    let Some(token) = token else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(principal) = store.authenticate(token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(PeerPrincipal {
        profile_id: principal.profile_id,
        device_id: principal.device_id,
    });
    next.run(request).await
}

struct PeerServer {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(store: AuthStore, path: std::path::PathBuf) -> PeerServer {
    let peer = PeerStore::open(path).expect("peer store");
    let app = Router::new()
        .merge(router(peer))
        .layer(middleware::from_fn_with_state(store, authenticated));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    PeerServer {
        base: format!("http://{address}"),
        task,
    }
}

fn token(store: &AuthStore, identity: &DeviceIdentity, principal: &Principal) -> String {
    let challenge = store
        .create_challenge(&principal.profile_id, &principal.device_id)
        .expect("challenge");
    let proof = identity
        .sign_challenge(&challenge)
        .expect("challenge proof");
    store.authenticate_challenge(&proof).expect("token").token
}

fn engine(dir: &TempDir, device: &str, base: &str, bearer: &str) -> EngineCore {
    std::fs::write(dir.path().join("device-id"), device).expect("device id");
    let edge =
        kratos_engine::doc_host::EdgeConfig::with_static_token(base, bearer).with_device(device);
    EngineCore::assemble_with_identity(
        dir.path(),
        Arc::new(HarnessRegistry::new()),
        kratos_proto::HarnessId::Mock,
        Some(edge),
        "org-peer",
        "user-peer",
    )
    .expect("engine core")
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

#[tokio::test]
async fn authenticated_engines_converge_registry_chat_queue_and_recover() {
    let temp = tempfile::tempdir().unwrap();
    let auth = AuthStore::open(temp.path().join("auth.sqlite")).unwrap();
    let owner_key = DeviceIdentity::load_or_create(temp.path().join("owner.key")).unwrap();
    let member_key = DeviceIdentity::load_or_create(temp.path().join("member.key")).unwrap();
    let owner = auth
        .create_profile(&owner_key.public_key(), Some("owner"))
        .unwrap();
    let invite = auth
        .create_invite(&owner.profile_id, &owner.device_id)
        .unwrap();
    let member_request = member_key
        .redeem_request(&invite, Some("member".into()))
        .unwrap();
    let member = auth.redeem_invite(&member_request).unwrap();
    let owner_token = token(&auth, &owner_key, &owner);
    let member_token = token(&auth, &member_key, &member);
    let server = serve(auth.clone(), temp.path().join("peer.sqlite")).await;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = engine(&dir_a, &owner.device_id, &server.base, &owner_token);
    let b = engine(&dir_b, &member.device_id, &server.base, &member_token);

    // The normal application RPC path creates the shared registry rows.
    let rpc_a = memory_client(a.rpc_service());
    rpc_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op":"createSpace", "spaceId":"space-1", "deviceId":owner.device_id,
                "path":"/tmp/peer", "gitDetected":false
            }),
        )
        .await
        .expect("create space");
    rpc_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op":"createChat", "chatId":"chat-1", "spaceId":"space-1"
            }),
        )
        .await
        .expect("create chat");
    wait_for(|| b.workspace.chat("chat-1").ok().flatten().is_some()).await;
    wait_for(|| {
        b.workspace
            .read_spaces()
            .unwrap_or_default()
            .iter()
            .any(|s| s.id == "space-1")
    })
    .await;

    // Both chat handles use the production EngineChatSink/ChatClient wiring.
    let host_chat = a.doc_host.open("chat-1").unwrap();
    let viewer_chat = b.doc_host.open("chat-1").unwrap();
    wait_for(|| host_chat.connected() && viewer_chat.connected()).await;

    // A viewer writes a durable queue row; the host receives it through chat2.
    let queued_id = b
        .doc_host
        .queue_message("chat-1", "queued before reconnect", vec![])
        .unwrap();
    wait_for(|| {
        host_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .any(|q| q.id == queued_id)
    })
    .await;

    // Host-authoritative edit lease, renewal, commit, reorder, and removal.
    let lease = match a
        .doc_host
        .begin_queued_message_edit("chat-1", &queued_id, &member.device_id, "window-1")
        .await
        .unwrap()
    {
        kratos_engine::doc_host::BeginQueueEditOutcome::Acquired {
            lease_id,
            base_text_hash,
            ..
        } => (lease_id, base_text_hash),
        other => panic!("expected edit lease, got {other:?}"),
    };
    assert!(matches!(
        a.doc_host
            .renew_queued_message_edit("chat-1", &queued_id, &lease.0)
            .await
            .unwrap(),
        kratos_engine::doc_host::RenewQueueEditOutcome::Renewed { .. }
    ));
    assert!(matches!(
        a.doc_host
            .finish_queued_message_edit(
                "chat-1",
                &queued_id,
                &lease.0,
                kratos_engine::doc_host::FinishQueueEditAction::Commit,
                Some("edited on host"),
                Some(&lease.1)
            )
            .await
            .unwrap(),
        kratos_engine::doc_host::FinishQueueEditOutcome::Committed
    ));
    wait_for(|| {
        viewer_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .any(|q| q.id == queued_id && q.text == "edited on host")
    })
    .await;

    let second = b
        .doc_host
        .queue_message("chat-1", "remove me", vec![])
        .unwrap();
    wait_for(|| {
        host_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .any(|q| q.id == second)
    })
    .await;
    assert!(
        a.doc_host
            .move_queued_message("chat-1", &second, 0)
            .unwrap()
    );
    assert!(
        a.doc_host
            .remove_queued_message("chat-1", &second)
            .await
            .unwrap()
    );
    wait_for(|| {
        !viewer_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .any(|q| q.id == second)
    })
    .await;

    // Restarting the host keeps the registry and chat snapshot recoverable.
    a.shutdown().await;
    drop(a);
    let reopened = engine(&dir_a, &owner.device_id, &server.base, &owner_token);
    let reopened_chat = reopened.doc_host.open("chat-1").unwrap();
    assert!(reopened.workspace.chat("chat-1").unwrap().is_some());
    assert!(
        reopened_chat
            .doc()
            .read_queue()
            .unwrap_or_default()
            .iter()
            .any(|q| q.id == queued_id && q.text == "edited on host")
    );

    // Peer management and local identity are forbidden through the remote
    // wrapper even though ordinary queue/registry methods are forwardable.
    let remote = memory_client(
        Arc::new(kratos_engine::rpc::RemoteEngineRpc(reopened.rpc_service())) as Arc<dyn RpcService>,
    );
    assert!(
        remote
            .call(methods::LOCAL_DEVICE, serde_json::json!({}))
            .await
            .is_err()
    );
    assert!(
        remote
            .call(methods::PEER_DEVICES, serde_json::json!({}))
            .await
            .is_err()
    );
    assert!(
        remote
            .call(
                methods::PEER_REVOKE,
                serde_json::json!({"deviceId":member.device_id})
            )
            .await
            .is_err()
    );

    reopened.shutdown().await;
    b.shutdown().await;
}
