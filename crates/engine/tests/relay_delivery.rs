//! Peer-relay delivery fallback (durable-by-design §Phase 3): engine A queues
//! a command for a chat hosted on engine B, A's chat2 rows can NEVER reach an
//! edge (none is configured — the 03:45 incident shape, rows dark while the
//! peer link lives), so after the rows grace A relay-forwards the entry over
//! the device-room link. B claims the client-minted id in its processed
//! ledger before executing — so when the doc row later "arrives" (simulated
//! by writing the same entry into B's doc), the drain dedupes it to a no-op:
//! exactly-once across both roads.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a
// full `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{post, put};
use axum::{Json, Router};
use base64::Engine as _;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use kratos_doc::{MessageRole, MessageStatus, SessionCommandPayload};
use kratos_engine::{EdgeConfig, EngineCore, HarnessRegistry};
use kratos_harness::{Harness, HarnessError, RunControls};
use kratos_proto::{
    AgentEvent, Device, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use kratos_rpc::{
    DeviceFrameHeader, LinkCache, LinkCacheConfig, StaticToken, decode_device_frame,
    encode_device_frame, methods,
};

const CHAT: &str = "chat-relay-fallback";

// Minimal in-memory device room (route-only subset of the DO semantics) —
// same shape as device_routing.rs.
#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Vec<u8>>>,
    clients: HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
}

async fn fake_device_room() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let state = Arc::new(Mutex::new(RelayState::default()));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let mut uri = String::new();
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &WsRequest, res: WsResponse| {
                        uri = req.uri().to_string();
                        Ok(res)
                    },
                )
                .await
                else {
                    return;
                };
                let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
                let is_host = query.contains("role=host");
                let conn_id = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("connId="))
                    .unwrap_or("anon")
                    .to_string();
                let (mut sink, mut ws_stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                {
                    let mut st = state.lock().expect("lock");
                    if is_host {
                        st.host = Some(tx);
                    } else {
                        st.clients.insert(conn_id.clone(), tx);
                    }
                }
                let writer = tokio::spawn(async move {
                    while let Some(bytes) = rx.recv().await {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(message)) = ws_stream.next().await {
                    let WsMessage::Binary(bytes) = message else {
                        continue;
                    };
                    let Ok((header, payload)) = decode_device_frame(&bytes) else {
                        break;
                    };
                    let st = state.lock().expect("lock");
                    if is_host {
                        let Some(to) = header.to else { continue };
                        if let Some(client) = st.clients.get(&to) {
                            let stripped = DeviceFrameHeader::new(header.s, header.k);
                            let _ = client
                                .send(encode_device_frame(&stripped, &payload).expect("encode"));
                        }
                    } else if let Some(host) = &st.host {
                        let mut routed = DeviceFrameHeader::new(header.s, header.k);
                        routed.from = Some(conn_id.clone());
                        let _ = host.send(encode_device_frame(&routed, &payload).expect("encode"));
                    }
                }
                writer.abort();
            });
        }
    });
    (url, task)
}

struct InstantHarness;

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-relay".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "relayed reply".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-relay".into()),
            }),
        ])
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble(dir, registry(), HarnessId::Mock, None).expect("engine assembles")
}

fn assemble_with_edge(
    dir: &std::path::Path,
    device_id: &str,
    edge: Option<EdgeConfig>,
) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble(dir, registry(), HarnessId::Mock, edge).expect("engine assembles")
}

#[derive(Default)]
struct CustodyState {
    bytes: Mutex<Vec<u8>>,
    init_count: AtomicUsize,
    chunk_count: AtomicUsize,
    commit_count: AtomicUsize,
    block_chunk_ack: AtomicBool,
    chunk_stored: tokio::sync::Notify,
    release_chunk_ack: tokio::sync::Notify,
}

async fn custody_init(State(state): State<Arc<CustodyState>>) -> Json<serde_json::Value> {
    state.init_count.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({
        "nextOffset": state.bytes.lock().unwrap().len(),
        "committed": false,
    }))
}

async fn custody_chunk(
    State(state): State<Arc<CustodyState>>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Json<serde_json::Value> {
    state.chunk_count.fetch_add(1, Ordering::SeqCst);
    let offset = query
        .get("offset")
        .and_then(|value| value.parse::<usize>().ok())
        .expect("chunk offset");
    let next = {
        let mut stored = state.bytes.lock().unwrap();
        if offset == stored.len() {
            stored.extend_from_slice(&body);
        }
        stored.len()
    };
    if state.block_chunk_ack.swap(false, Ordering::SeqCst) {
        state.chunk_stored.notify_one();
        state.release_chunk_ack.notified().await;
    }
    Json(serde_json::json!({ "nextOffset": next }))
}

async fn custody_commit(State(state): State<Arc<CustodyState>>) -> Json<serde_json::Value> {
    state.commit_count.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({ "committed": true }))
}

async fn custody_get(
    State(state): State<Arc<CustodyState>>,
    AxumPath(_upload): AxumPath<String>,
) -> Response {
    let bytes = state.bytes.lock().unwrap().clone();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let mut response = Response::new(axum::body::Body::from(bytes));
    response.headers_mut().insert(
        "x-attachment-digest",
        HeaderValue::from_str(&digest).unwrap(),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
}

async fn fake_custody_peer() -> (String, Arc<CustodyState>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(CustodyState {
        block_chunk_ack: AtomicBool::new(true),
        ..CustodyState::default()
    });
    let app = Router::new()
        .route("/attachment/{upload}", post(custody_init).get(custody_get))
        .route("/attachment/{upload}/chunk", put(custody_chunk))
        .route("/attachment/{upload}/commit", post(custody_commit))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind custody");
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), state, task)
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locally_assembled_engines_initialize_isolated_fallback_auth() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let dir_a = dirs.path().join("auth-a");
    let dir_b = dirs.path().join("auth-b");
    let core_a = Arc::new(assemble(&dir_a, "device-a"));
    let core_b = Arc::new(assemble(&dir_b, "device-b"));
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let open_a = {
        let core = core_a.clone();
        let barrier = barrier.clone();
        tokio::task::spawn_blocking(move || {
            barrier.wait();
            core.auth()
        })
    };
    let open_b = {
        let core = core_b.clone();
        tokio::task::spawn_blocking(move || {
            barrier.wait();
            core.auth()
        })
    };
    let (auth_a, auth_b) = tokio::join!(open_a, open_b);
    drop(auth_a.expect("open A fallback auth"));
    drop(auth_b.expect("open B fallback auth"));

    assert!(
        dir_a.join("peer/auth.sqlite").is_file(),
        "A's fallback auth belongs to A's data directory"
    );
    assert!(
        dir_b.join("peer/auth.sqlite").is_file(),
        "B's fallback auth belongs to B's data directory"
    );

    let core_a = Arc::try_unwrap(core_a).unwrap_or_else(|_| panic!("release core A"));
    let core_b = Arc::try_unwrap(core_b).unwrap_or_else(|_| panic!("release core B"));
    core_a.shutdown().await;
    core_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_dark_command_delivers_over_the_peer_relay_exactly_once() {
    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // Engine B hosts its device room on the fake relay.
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    let _host = core_b.start_host_relay(&relay_url, Arc::new(StaticToken("test-user".into())));

    // Engine A dials peers through the same relay — and has NO edge, so its
    // chat2 rows can never flush (the rows-dark half of the incident shape).
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    let mut link_config =
        LinkCacheConfig::new(relay_url.clone(), Arc::new(StaticToken("test-user".into())));
    link_config.probe_timeout = Duration::from_secs(5);
    core_a.set_links(LinkCache::new(link_config));

    // A knows the chat is hosted on B (local registry writes), and knows B's
    // stamped version passes the relay gate.
    core_a.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.2.12".into()),
        capabilities: kratos_proto::capabilities::current(),
    });
    let client_a = kratos_rpc::memory_client(core_a.rpc_service());
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": "device-b" }),
        )
        .await
        .expect("createChat on A");
    core_b
        .workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("pre-title on B (no auto-title harness run)");

    // The send: a durable local write on A. Rows go nowhere; the escort's
    // grace elapses; the entry crosses the peer link instead.
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: RunRequest {
            prompt: "over the relay".into(),
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
        message_id: "msg-relay-1".into(),
    })
    .expect("command json");
    client_a
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": CHAT, "command": command }),
        )
        .await
        .expect("queue on A");

    // B executes it — allow the 10s rows grace plus relay dial time.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if complete_assistant_count(&core_b) == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "relayed command never executed on B"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let entries = core_b
        .doc_host
        .open(CHAT)
        .expect("open on B")
        .doc()
        .read_entries()
        .expect("read entries");
    assert!(
        entries
            .iter()
            .any(|e| e.id == "msg-relay-1" && e.role == MessageRole::User),
        "B persisted the user message under the client-minted id"
    );

    // Exactly-once: the doc row "arrives" later over chat2 sync — simulate by
    // writing A's exact entry into B's doc, which kicks B's drain. The
    // processed ledger must dedupe it to a no-op.
    let entry = core_a
        .doc_host
        .open(CHAT)
        .expect("open on A")
        .doc()
        .read_commands()
        .expect("read A's commands")
        .into_iter()
        .next()
        .expect("A queued exactly one command");
    let handle_b = core_b.doc_host.open(CHAT).expect("open on B");
    handle_b
        .doc()
        .queue_command(&entry)
        .expect("simulate the synced doc row");
    // A second relay attempt (a retrying sender) must also dedupe.
    let dup = core_b
        .doc_host
        .ingest_relayed_command(CHAT, entry)
        .await
        .expect("duplicate relay accepted");
    assert_eq!(dup, "duplicate");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        complete_assistant_count(&core_b),
        1,
        "the doc row + a duplicate relay must not double-run the command"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_restarts_before_and_during_custody_upload_then_recovers_once() {
    let (relay_url, _relay) = fake_device_room().await;
    let (custody_url, custody, _custody_server) = fake_custody_peer().await;
    let dirs = tempfile::tempdir().expect("tempdir");
    let sender_dir = dirs.path().join("sender");
    let edge = EdgeConfig::with_static_token(&custody_url, "test-user");

    let core_b = assemble_with_edge(&dirs.path().join("target"), "device-b", Some(edge.clone()));
    let _host = core_b.start_host_relay(&relay_url, Arc::new(StaticToken("test-user".into())));

    // Queue durably while there is no custody transport. This is the restart
    // before upload: no Retry RPC is issued, and no in-memory escort survives.
    let core_a = assemble_with_edge(&sender_dir, "device-a", None);
    core_a.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.2.12".into()),
        capabilities: kratos_proto::capabilities::current(),
    });
    core_a
        .workspace
        .create_chat(CHAT, None, Some("device-b"), None, None)
        .expect("remote chat row");
    let bytes = b"restart-resumable attachment";
    core_a
        .uploads
        .append(
            "restart-upload",
            &base64::engine::general_purpose::STANDARD.encode(bytes),
            Some(0),
        )
        .expect("stage attachment");
    core_a
        .uploads
        .commit("restart-upload", "proof.png")
        .expect("commit local attachment");
    let pending = kratos_engine::uploads::pending_ref("restart-upload", "proof.png");
    core_a
        .doc_host
        .queue_command_with_transfers(
            CHAT,
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: format!("inspect\n- {pending}"),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "~".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: vec![pending],
                    worktree: None,
                    resume: None,
                },
                message_id: "msg-restart-custody".into(),
            },
            vec![kratos_engine::uploads::AttachmentTransfer {
                upload_id: "restart-upload".into(),
                file_name: "proof.png".into(),
            }],
        )
        .expect("durably queue attachment command");
    core_a.shutdown().await;
    drop(core_a);
    assert_eq!(custody.init_count.load(Ordering::SeqCst), 0);

    // Startup discovery re-arms the restored row. The peer stores the chunk
    // but withholds its ACK, placing shutdown inside the custody upload.
    let core_a = assemble_with_edge(&sender_dir, "device-a", Some(edge.clone()));
    let mut links =
        LinkCacheConfig::new(relay_url.clone(), Arc::new(StaticToken("test-user".into())));
    links.probe_timeout = Duration::from_secs(5);
    core_a.set_links(LinkCache::new(links));
    tokio::time::timeout(Duration::from_secs(5), custody.chunk_stored.notified())
        .await
        .expect("first runtime reached in-flight chunk");
    core_a.shutdown().await;
    drop(core_a);
    custody.release_chunk_ack.notify_one();

    // A second ordinary runtime start must ask for status, resume at the
    // peer's saved offset, commit, and relay the command exactly once.
    let core_a = assemble_with_edge(&sender_dir, "device-a", Some(edge));
    let mut links =
        LinkCacheConfig::new(relay_url.clone(), Arc::new(StaticToken("test-user".into())));
    links.probe_timeout = Duration::from_secs(5);
    core_a.set_links(LinkCache::new(links));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while complete_assistant_count(&core_b) != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "restored attachment command never executed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(complete_assistant_count(&core_b), 1);
    assert_eq!(custody.init_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        custody.chunk_count.load(Ordering::SeqCst),
        1,
        "the restarted sender must resume from the persisted peer offset"
    );
    assert_eq!(custody.commit_count.load(Ordering::SeqCst), 1);

    core_a.shutdown().await;
    core_b.shutdown().await;
}
