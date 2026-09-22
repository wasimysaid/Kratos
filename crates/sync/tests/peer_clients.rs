//! End-to-end client coverage for the durable loopback peer surface.
//!
//! These tests deliberately use the real `PeerStore` router and the real
//! `ChatClient`/`RegistryClient`.  The transport tests install a principal
//! directly because `zeron-sync` cannot depend on the engine's `AuthStore`
//! without creating a dependency cycle.  Ed25519 pairing and bearer
//! revocation are covered by `zeron-engine`'s `peer_auth` tests; this file
//! proves the authenticated principal boundary as seen by the data plane.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use futures::future::BoxFuture;
use reqwest::StatusCode;
use sha2::Digest as _;
use tempfile::TempDir;
use zeron_doc::RegistryDoc;
use zeron_proto::Chat;
use zeron_sync::chat_client::{ChatClient, ChatDocSink, CheckpointFetcher, RowImportOutcome};
use zeron_sync::peer::{PeerPrincipal, PeerStore, router};
use zeron_sync::{RegistryClient, SyncError};

async fn principal(mut req: Request<Body>, next: Next) -> Response {
    // Hyper probes a freshly accepted listener at `/`; it is not a protocol
    // request and should reach the router's ordinary 404 without test auth.
    if req.uri().path() == "/" {
        return next.run(req).await;
    }
    // HTTP callers use headers.  WebSocket clients cannot add headers through
    // the public client API, so the test URL carries the same test principal
    // in query parameters.  This is only a test-local authentication adapter.
    let query = req.uri().query().unwrap_or_default();
    let query_value = |name: &str| {
        query.split('&').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
    };
    let profile = req
        .headers()
        .get("x-profile")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| query_value("profile"))
        .expect("test principal profile");
    let device = req
        .headers()
        .get("x-device")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| query_value("device"))
        .expect("test principal device");
    req.extensions_mut().insert(PeerPrincipal {
        profile_id: profile,
        device_id: device,
    });
    next.run(req).await
}

async fn start(path: &Path) -> (String, PeerStore, tokio::task::JoinHandle<()>) {
    let store = PeerStore::open(path).expect("peer store");
    let app: Router = router(store.clone()).layer(middleware::from_fn(principal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (format!("http://{address}"), store, task)
}

fn room_url(base: &str, profile: &str, device: &str) -> String {
    format!("{base}?profile={profile}&device={device}")
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

struct MemorySink {
    doc: Mutex<loro::LoroDoc>,
    cursor: AtomicU64,
    pending: Mutex<Vec<(String, Vec<u8>)>>,
}

impl MemorySink {
    fn new(peer_id: u64) -> Self {
        let doc = loro::LoroDoc::new();
        doc.set_peer_id(peer_id).expect("peer id");
        Self {
            doc: Mutex::new(doc),
            cursor: AtomicU64::new(0),
            pending: Mutex::new(Vec::new()),
        }
    }

    fn update(&self, key: &str, value: &str) -> Vec<u8> {
        let doc = self.doc.lock().unwrap();
        doc.get_map("data").insert(key, value).expect("map write");
        doc.commit();
        doc.export(loro::ExportMode::all_updates()).expect("export")
    }

    fn has(&self, key: &str) -> bool {
        self.doc.lock().unwrap().get_map("data").get(key).is_some()
    }
}

impl ChatDocSink for MemorySink {
    fn pending_updates(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
        Ok(self.pending.lock().unwrap().clone())
    }

    fn persist_update(&self, batch_id: &str, bytes: &[u8]) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        if !pending.iter().any(|(id, _)| id == batch_id) {
            pending.push((batch_id.to_owned(), bytes.to_vec()));
        }
        Ok(())
    }

    fn acknowledge_update(&self, batch_id: &str) -> Result<(), String> {
        self.pending
            .lock()
            .unwrap()
            .retain(|(id, _)| id != batch_id);
        Ok(())
    }

    fn apply_row(&self, bytes: &[u8], cursor: u64) -> RowImportOutcome {
        let status = self.doc.lock().unwrap().import(bytes).expect("row imports");
        if status.pending.is_some() {
            return RowImportOutcome::PendingDependencies;
        }
        self.cursor.store(cursor, Ordering::SeqCst);
        RowImportOutcome::Applied
    }

    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        self.doc
            .lock()
            .unwrap()
            .import(bytes)
            .map_err(|e| e.to_string())?;
        self.cursor.store(cursor, Ordering::SeqCst);
        Ok(())
    }

    fn contains_frontier(&self, _frontier: &[u8]) -> bool {
        false
    }
    fn advance_cursor(&self, cursor: u64) {
        self.cursor.store(cursor, Ordering::SeqCst);
    }
}

struct EmptyCheckpoint;
impl CheckpointFetcher for EmptyCheckpoint {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        Box::pin(async { Err(SyncError::Protocol("unexpected checkpoint fetch".into())) })
    }
}

fn chat(id: &str, device_id: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: device_id.into(),
        title: Some(id.into()),
        archived: false,
        cwd: Some("/tmp".into()),
        branch: None,
        checkout_id: None,
        source_context: None,
        config: None,
        last_message_preview: None,
        last_message_at: None,
        created_at: chrono::Utc::now(),
        harness_session_id: None,
        harness_session_cwd: None,
        parent_chat_id: None,
        space_id: None,
        last_seen_at: None,
        room_gen: None,
    }
}

#[tokio::test]
async fn actual_clients_converge_over_peer_store_and_replay_is_deduped() {
    let temp = TempDir::new().unwrap();
    let (base, _store, task) = start(&temp.path().join("peer.sqlite")).await;
    let a = Arc::new(MemorySink::new(11));
    let b = Arc::new(MemorySink::new(22));
    let client_a = ChatClient::connect(
        &room_url(
            &format!("{base}/chat2/chat/ws").replacen("http", "ws", 1),
            "profile-a",
            "device-a",
        ),
        a.clone(),
        Arc::new(EmptyCheckpoint),
        "device-a",
        0,
    )
    .await
    .expect("chat A connects");
    let client_b = ChatClient::connect(
        &room_url(
            &format!("{base}/chat2/chat/ws").replacen("http", "ws", 1),
            "profile-a",
            "device-b",
        ),
        b.clone(),
        Arc::new(EmptyCheckpoint),
        "device-b",
        0,
    )
    .await
    .expect("chat B connects");

    let update = a.update("from-a", "hello");
    client_a.enqueue_batch("stable-batch".into(), update.clone());
    wait_until(|| b.has("from-a")).await;
    // The second HTTP-level delivery has the same stable id and bytes.  The
    // durable peer ledger must return dup rather than allocate another row.
    client_a.enqueue_batch("stable-batch".into(), update);
    wait_until(|| client_a.stats().pending_pushes == 0).await;
    let server_stats: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/chat2/chat/stats"))
        .header("x-profile", "profile-a")
        .header("x-device", "device-a")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        server_stats["headSeq"], 1,
        "batch replay must not create another row"
    );
    assert!(client_a.stats().connected && client_b.stats().connected);

    client_a.shutdown().await;
    client_b.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn actual_registry_client_syncs_rows_presence_and_profile_boundary_after_restart() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("peer.sqlite");
    let (base, store, task) = start(&db).await;
    let doc_a = Arc::new(Mutex::new(RegistryDoc::new("device-a")));
    let doc_b = Arc::new(Mutex::new(RegistryDoc::new("device-b")));
    doc_a
        .lock()
        .unwrap()
        .upsert_chat(&chat("chat-1", "device-a"))
        .unwrap();
    let client_a = RegistryClient::connect(
        &room_url(
            &format!("{base}/registry/org/ws").replacen("http", "ws", 1),
            "profile-a",
            "device-a",
        ),
        doc_a.clone(),
        "device-a",
    )
    .await
    .unwrap();
    client_a.nudge();
    wait_until(|| doc_a.lock().unwrap().pending_len() == 0).await;
    let client_b = RegistryClient::connect(
        &room_url(
            &format!("{base}/registry/org/ws").replacen("http", "ws", 1),
            "profile-a",
            "device-b",
        ),
        doc_b.clone(),
        "device-b",
    )
    .await
    .unwrap();
    wait_until(|| doc_b.lock().unwrap().chat("chat-1").unwrap().is_some()).await;
    client_a.set_presence(1234);
    wait_until(|| client_b.presence().get("device-a") == Some(&1234)).await;

    // A separate principal cannot read or merge profile-a's registry rows.
    let other = Arc::new(Mutex::new(RegistryDoc::new("other-device")));
    let other_client = RegistryClient::connect(
        &room_url(
            &format!("{base}/registry/org/ws").replacen("http", "ws", 1),
            "profile-b",
            "other-device",
        ),
        other.clone(),
        "other-device",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(other.lock().unwrap().read_chats().unwrap().is_empty());

    client_a.shutdown().await;
    client_b.shutdown().await;
    other_client.shutdown().await;
    task.abort();
    drop(store);

    // The same PeerStore database is durable across listener/process restart.
    let (base, _store, task) = start(&db).await;
    let restored = Arc::new(Mutex::new(RegistryDoc::new("restored")));
    let restored_client = RegistryClient::connect(
        &room_url(
            &format!("{base}/registry/org/ws").replacen("http", "ws", 1),
            "profile-a",
            "restored",
        ),
        restored.clone(),
        "restored",
    )
    .await
    .unwrap();
    wait_until(|| restored.lock().unwrap().chat("chat-1").unwrap().is_some()).await;
    restored_client.shutdown().await;
    task.abort();
}

#[tokio::test]
async fn attachment_custody_is_targeted_and_survives_sender_exit_and_restart() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("peer.sqlite");
    let bytes = b"sender deposits before going offline";
    let digest = format!("{:x}", sha2::Sha256::digest(bytes));
    let http = reqwest::Client::new();
    let (base, store, task) = start(&db).await;
    let auth = |request: reqwest::RequestBuilder, profile: &str, device: &str| {
        request
            .header("x-profile", profile)
            .header("x-device", device)
    };
    auth(
        http.post(format!("{base}/attachment/up-1"))
            .json(&serde_json::json!({
                "targetDevice":"host", "fileName":"note.txt", "length":bytes.len(), "digest":digest
            })),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    auth(
        http.put(format!(
            "{base}/attachment/up-1/chunk?targetDevice=host&offset=0"
        ))
        .body(bytes.as_slice()),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();

    auth(
        http.post(format!("{base}/attachment/up-1/commit?targetDevice=host")),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    task.abort();
    drop(store);

    let (base, _store, task) = start(&db).await;
    let status: serde_json::Value = auth(
        http.get(format!(
            "{base}/attachment/up-1/status?senderDevice=sender&targetDevice=host"
        )),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(status["nextOffset"], bytes.len());
    let fetched = auth(
        http.get(format!(
            "{base}/attachment/up-1?senderDevice=sender&targetDevice=host"
        )),
        "profile-a",
        "host",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(fetched.bytes().await.unwrap().as_ref(), bytes);
    let isolated = auth(
        http.get(format!(
            "{base}/attachment/up-1?senderDevice=sender&targetDevice=host"
        )),
        "profile-b",
        "host",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(isolated.status(), StatusCode::NOT_FOUND);
    task.abort();
}
