//! Durable, self-hosted replacement for the active edge sync/storage rooms.
//! Authentication is intentionally external: every data handler requires a
//! [`PeerPrincipal`] request extension installed by the engine middleware.

mod attachment;
pub mod preview;

mod store;

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use kratos_doc::RowOp;

use store::{MAX_CHECKPOINT_BYTES, MAX_SIDECAR_BYTES, MAX_TOOL_BLOB_BYTES};
pub use store::{PeerStore, PeerStoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPrincipal {
    pub profile_id: String,
    pub device_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnKind {
    Chat,
    Registry,
    DeviceHost,
    DeviceClient,
}

struct LiveConn {
    profile: String,
    principal_device: String,
    room: String,
    kind: ConnKind,
    conn_id: String,
    tx: mpsc::UnboundedSender<Message>,
}

#[derive(Default)]
pub(crate) struct LiveState {
    state: Mutex<LiveEntries>,
    next: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct LiveEntries {
    conns: HashMap<u64, LiveConn>,
    revoked: std::collections::HashSet<(String, String)>,
}

impl LiveState {
    fn add(&self, conn: LiveConn) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .revoked
            .contains(&(conn.profile.clone(), conn.principal_device.clone()))
        {
            let _ = conn.tx.send(Message::Close(Some(CloseFrame {
                code: 4403,
                reason: "device revoked".into(),
            })));
            return id;
        }
        state.conns.insert(id, conn);
        id
    }
    fn remove(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .conns
            .remove(&id);
    }
    fn send_where(
        &self,
        mut predicate: impl FnMut(&LiveConn) -> bool,
        message: Message,
        skip: Option<u64>,
    ) -> usize {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut sent = 0;
        for (id, c) in state.conns.iter() {
            if Some(*id) != skip && predicate(c) && c.tx.send(message.clone()).is_ok() {
                sent += 1;
            }
        }
        sent
    }
    fn revoke(&self, profile: &str, device: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .revoked
            .insert((profile.to_owned(), device.to_owned()));
        let ids: Vec<_> = state
            .conns
            .iter()
            .filter_map(|(id, conn)| {
                (conn.profile == profile && conn.principal_device == device).then_some(*id)
            })
            .collect();
        for id in ids {
            if let Some(conn) = state.conns.remove(&id) {
                let _ = conn.tx.send(Message::Close(Some(CloseFrame {
                    code: 4403,
                    reason: "device revoked".into(),
                })));
            }
        }
    }
    fn count(&self, profile: &str, room: &str, kind: ConnKind) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .conns
            .values()
            .filter(|c| c.profile == profile && c.room == room && c.kind == kind)
            .count()
    }
}

/// Build the protocol-compatible peer surface. The caller should layer auth
/// middleware outside this router and insert `PeerPrincipal` per request.
pub fn router(state: PeerStore) -> Router {
    Router::new()
        .route("/chat2/{id}/ws", get(chat_ws))
        .route("/chat2/{id}/rows", get(chat_rows_get).post(chat_rows_post))
        .route(
            "/chat2/{id}/checkpoint",
            get(checkpoint_get).post(checkpoint_post),
        )
        .route(
            "/chat2/{id}/tail",
            get(chat_sidecar_get).put(chat_sidecar_put),
        )
        .route(
            "/chat2/{id}/diff",
            get(chat_sidecar_get).put(chat_sidecar_put),
        )
        .route("/chat2/{id}/stats", get(chat_stats))
        .route("/registry/{org}/ws", get(registry_ws))
        .route("/registry/{org}/rows", get(registry_rows))
        .route("/registry/{org}/push", post(registry_push))
        .route("/registry/{org}/stats", get(registry_stats))
        .route("/device/{id}/ws", get(device_ws))
        .route("/device/{id}/nudge", post(device_nudge))
        .route("/device/{id}/status", get(device_status))
        .route(
            "/device/{id}/sidecar/{name}",
            get(device_sidecar_get).post(device_sidecar_post),
        )
        .route(
            "/attachment/{upload}",
            post(attachment_init).get(attachment_get),
        )
        .route("/attachment/{upload}/status", get(attachment_status))
        .route(
            "/attachment/{upload}/chunk",
            axum::routing::put(attachment_chunk),
        )
        .route("/attachment/{upload}/commit", post(attachment_commit))
        .route(
            "/blob/{chat}/{part}",
            get(blob_get).head(blob_head).put(blob_put),
        )
        .layer(DefaultBodyLimit::max(MAX_CHECKPOINT_BYTES))
        .with_state(state)
}

type ApiResult<T = Response> = Result<T, ApiError>;
struct ApiError(StatusCode, Value);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(self.1)).into_response()
    }
}
impl From<PeerStoreError> for ApiError {
    fn from(e: PeerStoreError) -> Self {
        tracing::error!(error=%e,"peer store failure");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"storage"}),
        )
    }
}
fn bad(error: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, json!({"error":error}))
}
fn valid_id(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:@/-".contains(&b))
}

fn valid_part_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:#~-".contains(&b))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ChatQuery {
    after: Option<u64>,
    batch_id: Option<String>,
    exclude_own: Option<u8>,
}

async fn chat_rows_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    Query(q): Query<ChatQuery>,
) -> ApiResult {
    if !valid_id(&chat, 256) {
        return Err(bad("bad_chat_id"));
    }
    let stats = store.chat_stats(&p.profile_id, &chat)?;
    let frontier = store.chat_frontier(&p.profile_id, &chat)?;
    let mut frames = vec![crate::chat_frames::encode(
        crate::chat_frames::frame_type::STATE,
        &json!({"headSeq":stats.head_seq,"seqFloor":stats.seq_floor,"checkpointSeq":stats.checkpoint_seq,"checkpointSize":stats.checkpoint_size,"rowCount":stats.row_count,"rowBytes":stats.row_bytes}),
        &frontier,
    )];
    let exclude = if q.exclude_own == Some(1) {
        Some(p.device_id.as_str())
    } else {
        None
    };
    for row in store.chat_rows(&p.profile_id, &chat, q.after.unwrap_or(0), exclude)? {
        frames.push(crate::chat_frames::encode(
            crate::chat_frames::frame_type::ROW,
            &json!({"seq":row.seq,"device":row.device,"batchId":row.batch_id}),
            &row.bytes,
        ));
    }
    frames.push(crate::chat_frames::encode(
        crate::chat_frames::frame_type::ROWS_DONE,
        &json!({"headSeq":stats.head_seq}),
        &[],
    ));
    let mut body = Vec::new();
    for f in frames {
        body.extend_from_slice(&(f.len() as u32).to_le_bytes());
        body.extend(f);
    }
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}
async fn chat_rows_post(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    Query(q): Query<ChatQuery>,
    body: Bytes,
) -> ApiResult {
    let batch = q
        .batch_id
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or_else(|| bad("bad_push"))?;
    let out = store
        .append_chat(&p.profile_id, &chat, &p.device_id, &batch, &body)
        .map_err(store_input_error)?;
    if !out.dup {
        let frame = crate::chat_frames::encode(
            crate::chat_frames::frame_type::ROW,
            &json!({"seq":out.seq,"device":p.device_id,"batchId":batch}),
            &body,
        );
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == chat && c.kind == ConnKind::Chat,
            Message::Binary(frame.into()),
            None,
        );
    }
    Ok(axum::Json(json!({"batchId":batch,"seq":out.seq,"dup":out.dup})).into_response())
}
fn store_input_error(e: PeerStoreError) -> ApiError {
    match e {
        PeerStoreError::Invalid(code) => {
            let status = if code == "too_large" {
                StatusCode::PAYLOAD_TOO_LARGE
            } else if code == "backlog_full" {
                StatusCode::INSUFFICIENT_STORAGE
            } else if code == "floor_regression" || code == "ahead_of_head" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError(status, json!({"error":code}))
        }
        e => e.into(),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CheckpointQuery {
    seq_covered: Option<u64>,
}
async fn checkpoint_post(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    Query(q): Query<CheckpointQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    if body.len() > MAX_CHECKPOINT_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"too_large"}),
        ));
    }
    let seq = q.seq_covered.ok_or_else(|| bad("bad_seq_covered"))?;
    let encoded = headers
        .get("x-chat2-frontier")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| bad("bad_frontier"))?;
    let frontier = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| bad("bad_frontier"))?;
    let (floor, pruned) = store
        .commit_checkpoint(&p.profile_id, &chat, seq, &frontier, &body)
        .map_err(store_input_error)?;
    Ok(axum::Json(json!({"ok":true,"seqFloor":floor,"pruned":pruned})).into_response())
}
async fn checkpoint_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let Some((seq, bytes)) = store.checkpoint(&p.profile_id, &chat)? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            json!({"error":"not_found"}),
        ));
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);
    if let Some(start) = range {
        if start >= bytes.len() {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{}", bytes.len())).unwrap(),
            );
            return Ok(r);
        }
    }
    let start = range.unwrap_or(0);
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let total = bytes.len();
    let body = bytes[start..].to_vec();
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        "x-chat2-checkpoint-seq",
        HeaderValue::from_str(&seq.to_string()).unwrap(),
    );
    if range.is_some() {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{}/{total}", total - 1)).unwrap(),
        );
    }
    Ok(r)
}
fn parse_range(s: &str) -> Option<usize> {
    let n = s.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok()?;
    if n > 0 { Some(n) } else { None }
}

async fn chat_sidecar_put(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> ApiResult {
    if body.len() > MAX_SIDECAR_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"too_large"}),
        ));
    }
    let name = if uri.path().ends_with("/tail") {
        "tail"
    } else {
        "diff"
    };
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");
    store.put_sidecar(&p.profile_id, "chat", &chat, name, ct, &body)?;
    Ok(axum::Json(json!({"ok":true,"bytes":body.len()})).into_response())
}
async fn chat_sidecar_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
    uri: axum::http::Uri,
) -> ApiResult {
    let name = if uri.path().ends_with("/tail") {
        "tail"
    } else {
        "diff"
    };
    sidecar_response(store.get_sidecar(&p.profile_id, "chat", &chat, name)?)
}
async fn chat_stats(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
) -> ApiResult {
    let s = store.chat_stats(&p.profile_id, &chat)?;
    Ok(axum::Json(json!({"headSeq":s.head_seq,"seqFloor":s.seq_floor,"checkpointSeq":s.checkpoint_seq,"checkpointSize":s.checkpoint_size,"rowCount":s.row_count,"rowBytes":s.row_bytes,"connectedSockets":store.inner.live.count(&p.profile_id,&chat,ConnKind::Chat)})).into_response())
}

async fn chat_ws(
    ws: WebSocketUpgrade,
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(chat): Path<String>,
) -> ApiResult {
    if !valid_id(&chat, 256) {
        return Err(bad("bad_chat_id"));
    }
    Ok(ws
        .on_upgrade(move |socket| chat_socket(socket, store, p, chat))
        .into_response())
}
async fn chat_socket(socket: WebSocket, store: PeerStore, p: PeerPrincipal, chat: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let id = store.inner.live.add(LiveConn {
        profile: p.profile_id.clone(),
        principal_device: p.device_id.clone(),
        room: chat.clone(),
        kind: ConnKind::Chat,
        conn_id: String::new(),
        tx,
    });
    loop {
        tokio::select! {out=rx.recv()=>match out{Some(m)=>if sink.send(m).await.is_err(){break},None=>break},incoming=stream.next()=>{let Some(Ok(msg))=incoming else{break};match msg{Message::Text(t) if t.as_str()=="ping"=>{let _=sink.send(Message::Text("pong".into())).await;}Message::Binary(bytes)=>{if let Some(frame)=crate::chat_frames::decode(&bytes){match frame.kind{crate::chat_frames::frame_type::HELLO=>{let s=store.chat_stats(&p.profile_id,&chat);if let Ok(s)=s{let frontier=store.chat_frontier(&p.profile_id,&chat).unwrap_or_default();let f=crate::chat_frames::encode(crate::chat_frames::frame_type::STATE,&json!({"headSeq":s.head_seq,"seqFloor":s.seq_floor,"checkpointSeq":s.checkpoint_seq,"checkpointSize":s.checkpoint_size,"rowCount":s.row_count,"rowBytes":s.row_bytes}),&frontier);let _=sink.send(Message::Binary(f.into())).await;}}crate::chat_frames::frame_type::ROWS_REQ=>{let after=frame.header["after"].as_u64().unwrap_or(0);let exclude=frame.header["excludeOwn"].as_bool().unwrap_or(false).then_some(p.device_id.as_str());if let Ok(rows)=store.chat_rows(&p.profile_id,&chat,after,exclude){for row in rows{let f=crate::chat_frames::encode(crate::chat_frames::frame_type::ROW,&json!({"seq":row.seq,"device":row.device,"batchId":row.batch_id}),&row.bytes);if sink.send(Message::Binary(f.into())).await.is_err(){break;}}let head=store.chat_stats(&p.profile_id,&chat).map(|s|s.head_seq).unwrap_or(0);let f=crate::chat_frames::encode(crate::chat_frames::frame_type::ROWS_DONE,&json!({"headSeq":head}),&[]);let _=sink.send(Message::Binary(f.into())).await;}}crate::chat_frames::frame_type::PUSH=>{let batch=frame.header["batchId"].as_str().unwrap_or("");match store.append_chat(&p.profile_id,&chat,&p.device_id,batch,&frame.payload){Ok(out)=>{if !out.dup{let f=crate::chat_frames::encode(crate::chat_frames::frame_type::ROW,&json!({"seq":out.seq,"device":p.device_id,"batchId":batch}),&frame.payload);store.inner.live.send_where(|c|c.profile==p.profile_id&&c.room==chat&&c.kind==ConnKind::Chat,Message::Binary(f.into()),Some(id));}let ack=crate::chat_frames::encode(crate::chat_frames::frame_type::ACK,&json!({"batchId":batch,"seq":out.seq,"dup":out.dup}),&[]);let _=sink.send(Message::Binary(ack.into())).await;}Err(e)=>{let code=match e{PeerStoreError::Invalid(c)=>c,_=>"storage"};let f=crate::chat_frames::encode(crate::chat_frames::frame_type::ERROR,&json!({"code":code,"message":"push rejected","batchId":batch}),&[]);let _=sink.send(Message::Binary(f.into())).await;}}}crate::chat_frames::frame_type::PRESENCE=>{let f=crate::chat_frames::encode(crate::chat_frames::frame_type::PRESENCE,&json!({"device":p.device_id,"at":frame.header["at"]}),&frame.payload);store.inner.live.send_where(|c|c.profile==p.profile_id&&c.room==chat&&c.kind==ConnKind::Chat,Message::Binary(f.into()),Some(id));}crate::chat_frames::frame_type::PROBE=>{let head=store.chat_stats(&p.profile_id,&chat).map(|s|s.head_seq).unwrap_or(0);let f=crate::chat_frames::encode(crate::chat_frames::frame_type::PROBE_OK,&json!({"headSeq":head}),&[]);let _=sink.send(Message::Binary(f.into())).await;}_=>{}}}}Message::Close(_)=>break,_=>{}}}}
    }
    store.inner.live.remove(id);
}

#[derive(Deserialize, Default)]
struct RegistryQuery {
    since: Option<u64>,
    beat: Option<u8>,
}
fn valid_hlc(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 22
        && b.get(13) == Some(&b'-')
        && b.get(20) == Some(&b'-')
        && b[..13].iter().all(u8::is_ascii_digit)
        && b[14..20].iter().all(u8::is_ascii_digit)
        && b[21..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}
fn validate_ops(ops: &[RowOp]) -> Result<(), ApiError> {
    if ops.is_empty() || ops.len() > 500 {
        return Err(bad("bad_push"));
    }
    for op in ops {
        if !valid_id(&op.kind, 32)
            || !valid_id(&op.id, 256)
            || !valid_hlc(&op.hlc)
            || serde_json::to_vec(op).map_or(true, |b| b.len() > 16 * 1024)
        {
            return Err(bad("invalid_op"));
        }
        if let Some(cs) = &op.clocks {
            if cs.values().any(|v| !valid_hlc(v)) {
                return Err(bad("invalid_op"));
            }
        }
    }
    Ok(())
}
async fn registry_rows(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(org): Path<String>,
    Query(q): Query<RegistryQuery>,
) -> ApiResult {
    let (seq, full, gc, rows) = store.registry_state(&p.profile_id, &org, q.since)?;
    if q.beat == Some(1) {
        let frame = Message::Text(
            json!({"t":"presence","device":p.device_id,"at":now_ms()})
                .to_string()
                .into(),
        );
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == org && c.kind == ConnKind::Registry,
            frame,
            None,
        );
    }
    Ok(
        axum::Json(json!({"seq":seq,"full":full,"gcFloor":gc,"rows":rows,"presence":{}}))
            .into_response(),
    )
}
#[derive(Deserialize)]
struct PushBody {
    batch: String,
    ops: Vec<RowOp>,
}
async fn registry_push(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(org): Path<String>,
    axum::Json(body): axum::Json<PushBody>,
) -> ApiResult {
    validate_ops(&body.ops)?;
    if body.batch.is_empty() || body.batch.len() > 128 {
        return Err(bad("bad_push"));
    }
    let out = store.apply_registry(&p.profile_id, &org, &body.batch, &body.ops)?;
    if !out.rows.is_empty() {
        let m = Message::Text(
            json!({"t":"rows","seq":out.seq,"rows":out.rows})
                .to_string()
                .into(),
        );
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == org && c.kind == ConnKind::Registry,
            m,
            None,
        );
    }
    Ok(axum::Json(json!({"batch":out.batch,"seq":out.seq,"applied":out.applied})).into_response())
}
async fn registry_stats(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(org): Path<String>,
) -> ApiResult {
    let (seq, _, gc, rows) = store.registry_state(&p.profile_id, &org, None)?;
    Ok(axum::Json(json!({"seq":seq,"gcFloor":gc,"rowCount":rows.len(),"connectedSockets":store.inner.live.count(&p.profile_id,&org,ConnKind::Registry)})).into_response())
}
async fn registry_ws(
    ws: WebSocketUpgrade,
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(org): Path<String>,
) -> ApiResult {
    Ok(ws
        .on_upgrade(move |socket| registry_socket(socket, store, p, org))
        .into_response())
}
async fn registry_socket(socket: WebSocket, store: PeerStore, p: PeerPrincipal, org: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let id = store.inner.live.add(LiveConn {
        profile: p.profile_id.clone(),
        principal_device: p.device_id.clone(),
        room: org.clone(),
        kind: ConnKind::Registry,
        conn_id: String::new(),
        tx,
    });
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(message) => {
                    if sink.send(message).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) if text.as_str() == "ping" => {
                        let _ = sink.send(Message::Text("pong".into())).await;
                    }
                    Message::Text(text) => {
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else { break };
                        match frame["t"].as_str() {
                            Some("hello") => {
                                let cursor = frame["cursor"].as_u64();
                                if let Ok((seq, full, gc, rows)) = store.registry_state(&p.profile_id, &org, cursor) {
                                    let state = json!({"t":"state","seq":seq,"full":full,"gcFloor":gc,"rows":rows,"presence":{}});
                                    let _ = sink.send(Message::Text(state.to_string().into())).await;
                                }
                            }
                            Some("push") => {
                                let batch = frame["batch"].as_str();
                                let ops = serde_json::from_value::<Vec<RowOp>>(frame["ops"].clone());
                                if let (Some(batch), Ok(ops)) = (batch, ops)
                                    && validate_ops(&ops).is_ok()
                                    && let Ok(out) = store.apply_registry(&p.profile_id, &org, batch, &ops)
                                {
                                    if !out.rows.is_empty() {
                                        let rows = Message::Text(json!({"t":"rows","seq":out.seq,"rows":out.rows}).to_string().into());
                                        // The sender must observe merged truth before its ack retires
                                        // the optimistic batch; peers receive the same frame.
                                        let _ = sink.send(rows.clone()).await;
                                        store.inner.live.send_where(
                                            |c| c.profile == p.profile_id && c.room == org && c.kind == ConnKind::Registry,
                                            rows,
                                            Some(id),
                                        );
                                    }
                                    let ack = json!({"t":"ack","batch":out.batch,"seq":out.seq,"applied":out.applied});
                                    let _ = sink.send(Message::Text(ack.to_string().into())).await;
                                }
                            }
                            Some("presence") => {
                                let message = Message::Text(json!({"t":"presence","device":p.device_id,"at":frame["at"]}).to_string().into());
                                store.inner.live.send_where(
                                    |c| c.profile == p.profile_id && c.room == org && c.kind == ConnKind::Registry,
                                    message,
                                    Some(id),
                                );
                            }
                            Some("probe") => {
                                let seq = store.registry_state(&p.profile_id, &org, Some(0)).map(|state| state.0).unwrap_or(0);
                                let reply = json!({"t":"probe-ok","seq":seq});
                                let _ = sink.send(Message::Text(reply.to_string().into())).await;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    store.inner.live.remove(id);
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeviceQuery {
    role: Option<String>,
    conn_id: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct DeviceHeader {
    s: String,
    k: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}
fn decode_device(bytes: &[u8]) -> Option<(DeviceHeader, Vec<u8>)> {
    let (mut n, mut shift, mut off) = (0usize, 0, 0);
    loop {
        let b = *bytes.get(off)?;
        off += 1;
        n |= ((b & 127) as usize) << shift;
        if b & 128 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 {
            return None;
        }
    }
    let end = off.checked_add(n)?;
    let h = serde_json::from_slice(bytes.get(off..end)?).ok()?;
    Some((h, bytes.get(end..)?.to_vec()))
}
fn encode_device(h: &DeviceHeader, payload: &[u8]) -> Vec<u8> {
    let j = serde_json::to_vec(h).unwrap();
    let mut n = j.len();
    let mut out = Vec::new();
    loop {
        let mut b = (n & 127) as u8;
        n >>= 7;
        if n > 0 {
            b |= 128
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
    out.extend(j);
    out.extend(payload);
    out
}
fn relay_error(stream: &str, code: &str) -> Message {
    Message::Binary(
        encode_device(
            &DeviceHeader {
                s: stream.into(),
                k: " relay".into(),
                to: None,
                from: None,
            },
            json!({"error":code}).to_string().as_bytes(),
        )
        .into(),
    )
}
async fn device_ws(
    ws: WebSocketUpgrade,
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(device): Path<String>,
    Query(q): Query<DeviceQuery>,
) -> ApiResult {
    let host = q.role.as_deref() == Some("host");
    if host && device != p.device_id {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ));
    }
    let conn = q
        .conn_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Ok(ws
        .on_upgrade(move |socket| device_socket(socket, store, p, device, host, conn))
        .into_response())
}
async fn device_socket(
    socket: WebSocket,
    store: PeerStore,
    p: PeerPrincipal,
    target: String,
    host: bool,
    conn_id: String,
) {
    let kind = if host {
        ConnKind::DeviceHost
    } else {
        ConnKind::DeviceClient
    };
    if host {
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == target && c.kind == ConnKind::DeviceHost,
            Message::Close(Some(CloseFrame {
                code: 4409,
                reason: "superseded".into(),
            })),
            None,
        );
    }
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let id = store.inner.live.add(LiveConn {
        profile: p.profile_id.clone(),
        principal_device: p.device_id.clone(),
        room: target.clone(),
        kind,
        conn_id: conn_id.clone(),
        tx,
    });
    if host {
        if let Ok(chats) = store.pending_nudges(&p.profile_id, &target) {
            for chat in chats {
                let payload = json!({"chatId":chat}).to_string();
                let m = Message::Binary(
                    encode_device(
                        &DeviceHeader {
                            s: chat.clone(),
                            k: "nudge".into(),
                            to: None,
                            from: None,
                        },
                        payload.as_bytes(),
                    )
                    .into(),
                );
                if sink.send(m).await.is_ok() {
                    let _ = store.ack_nudge(&p.profile_id, &target, &chat);
                }
            }
        }
    }
    loop {
        tokio::select! {out=rx.recv()=>match out{Some(m)=>if sink.send(m).await.is_err(){break},None=>break},incoming=stream.next()=>{let Some(Ok(msg))=incoming else{break};match msg{Message::Text(t) if t.as_str()=="ping"=>{let _=sink.send(Message::Text("pong".into())).await;}Message::Binary(bytes)=>{let Some((h,payload))=decode_device(&bytes)else{break};if host{if let Some(to)=h.to{let out=Message::Binary(encode_device(&DeviceHeader{s:h.s.clone(),k:h.k,to:None,from:None},&payload).into());let sent=store.inner.live.send_where(|c|c.profile==p.profile_id&&c.room==target&&c.kind==ConnKind::DeviceClient&&c.conn_id==to,out,None);if sent==0{let _=sink.send(relay_error(&h.s,"client_gone")).await;}}}else{let out=Message::Binary(encode_device(&DeviceHeader{s:h.s.clone(),k:h.k,to:None,from:Some(conn_id.clone())},&payload).into());let sent=store.inner.live.send_where(|c|c.profile==p.profile_id&&c.room==target&&c.kind==ConnKind::DeviceHost,out,None);if sent==0{let _=sink.send(relay_error(&h.s,"host_offline")).await;}}}Message::Close(_)=>break,_=>{}}}}
    }
    store.inner.live.remove(id);
    if host {
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == target && c.kind == ConnKind::DeviceClient,
            relay_error("", "host_closed"),
            None,
        );
    } else {
        let payload = json!({"error":"client_closed"}).to_string();
        let out = Message::Binary(
            encode_device(
                &DeviceHeader {
                    s: String::new(),
                    k: " relay".into(),
                    to: None,
                    from: Some(conn_id),
                },
                payload.as_bytes(),
            )
            .into(),
        );
        store.inner.live.send_where(
            |c| c.profile == p.profile_id && c.room == target && c.kind == ConnKind::DeviceHost,
            out,
            None,
        );
    }
}
async fn device_nudge(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(device): Path<String>,
    axum::Json(v): axum::Json<Value>,
) -> ApiResult {
    let chat = v["chatId"]
        .as_str()
        .filter(|s| valid_id(s, 64))
        .ok_or_else(|| bad("bad_chat_id"))?;
    store.queue_nudge(&p.profile_id, &device, chat)?;
    let payload = json!({"chatId":chat}).to_string();
    let m = Message::Binary(
        encode_device(
            &DeviceHeader {
                s: chat.into(),
                k: "nudge".into(),
                to: None,
                from: None,
            },
            payload.as_bytes(),
        )
        .into(),
    );
    let delivered = store.inner.live.send_where(
        |c| c.profile == p.profile_id && c.room == device && c.kind == ConnKind::DeviceHost,
        m,
        None,
    ) > 0;
    if delivered {
        store.ack_nudge(&p.profile_id, &device, chat)?;
    }
    Ok(axum::Json(json!({"delivered":delivered,"queued":!delivered})).into_response())
}
async fn device_status(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(device): Path<String>,
) -> ApiResult {
    Ok(axum::Json(json!({"hostConnected":store.inner.live.count(&p.profile_id,&device,ConnKind::DeviceHost)>0,"hostSockets":store.inner.live.count(&p.profile_id,&device,ConnKind::DeviceHost)})).into_response())
}
async fn device_sidecar_post(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path((device, name)): Path<(String, String)>,
    axum::Json(v): axum::Json<Value>,
) -> ApiResult {
    if device != p.device_id {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ));
    }
    let b = serde_json::to_vec(&v).map_err(|_| bad("bad_json"))?;
    store.put_sidecar(
        &p.profile_id,
        "device",
        &device,
        &name,
        "application/json",
        &b,
    )?;
    Ok(axum::Json(json!({"ok":true})).into_response())
}
async fn device_sidecar_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path((device, name)): Path<(String, String)>,
) -> ApiResult {
    sidecar_response(store.get_sidecar(&p.profile_id, "device", &device, &name)?)
}
fn sidecar_response(value: Option<(String, Vec<u8>)>) -> ApiResult {
    let Some((ct, b)) = value else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            json!({"error":"not_found"}),
        ));
    };
    let mut r = b.into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    Ok(r)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentInit {
    target_device: String,
    file_name: String,
    length: u64,
    digest: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentAccess {
    sender_device: String,
    target_device: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentWrite {
    target_device: String,
    #[serde(default)]
    offset: u64,
}

fn attachment_json(meta: attachment::CustodyMeta) -> Value {
    json!({"senderDevice":meta.sender,"targetDevice":meta.target,"fileName":meta.file_name,"length":meta.length,"digest":meta.digest,"committed":meta.committed,"nextOffset":meta.next_offset})
}

async fn attachment_init(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(upload): Path<String>,
    axum::Json(init): axum::Json<AttachmentInit>,
) -> ApiResult {
    if !valid_id(&upload, 64)
        || !valid_id(&init.target_device, 128)
        || init.file_name.is_empty()
        || init.file_name.len() > 256
    {
        return Err(bad("bad_metadata"));
    }
    let meta = store
        .init_attachment(
            &p.profile_id,
            &upload,
            &p.device_id,
            &init.target_device,
            &init.file_name,
            init.length,
            &init.digest,
        )
        .map_err(store_input_error)?;
    Ok(axum::Json(attachment_json(meta)).into_response())
}

async fn attachment_status(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(upload): Path<String>,
    Query(q): Query<AttachmentAccess>,
) -> ApiResult {
    if p.device_id != q.sender_device && p.device_id != q.target_device {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ));
    }
    let Some(meta) =
        store.attachment_meta(&p.profile_id, &upload, &q.sender_device, &q.target_device)?
    else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            json!({"error":"not_found"}),
        ));
    };
    Ok(axum::Json(attachment_json(meta)).into_response())
}

async fn attachment_chunk(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(upload): Path<String>,
    Query(q): Query<AttachmentWrite>,
    body: Bytes,
) -> ApiResult {
    let meta = store
        .put_attachment_chunk(
            &p.profile_id,
            &upload,
            &p.device_id,
            &q.target_device,
            q.offset,
            &body,
        )
        .map_err(store_input_error)?;
    Ok(axum::Json(attachment_json(meta)).into_response())
}

async fn attachment_commit(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(upload): Path<String>,
    Query(q): Query<AttachmentWrite>,
) -> ApiResult {
    let meta = store
        .commit_attachment(&p.profile_id, &upload, &p.device_id, &q.target_device)
        .map_err(store_input_error)?;
    Ok(axum::Json(attachment_json(meta)).into_response())
}

async fn attachment_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path(upload): Path<String>,
    Query(q): Query<AttachmentAccess>,
    headers: HeaderMap,
) -> ApiResult {
    if p.device_id != q.sender_device && p.device_id != q.target_device {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ));
    }
    let offset = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range)
        .map(|v| v as u64)
        .unwrap_or(q.offset);
    let Some((meta, bytes)) = store
        .read_attachment(
            &p.profile_id,
            &upload,
            &q.sender_device,
            &q.target_device,
            offset,
        )
        .map_err(store_input_error)?
    else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            json!({"error":"not_found"}),
        ));
    };
    let status = if offset > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut response = (status, bytes).into_response();
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        "x-attachment-name",
        HeaderValue::from_str(&meta.file_name).map_err(|_| bad("bad_metadata"))?,
    );
    response.headers_mut().insert(
        "x-attachment-digest",
        HeaderValue::from_str(&meta.digest).expect("validated digest is a header value"),
    );
    if offset > 0 {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {offset}-{}/{}",
                meta.length.saturating_sub(1),
                meta.length
            ))
            .expect("numeric content range"),
        );
    }
    Ok(response)
}

async fn blob_put(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path((chat, part)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    if body.len() > MAX_TOOL_BLOB_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"too_large"}),
        ));
    }
    if !valid_id(&chat, 256) || !valid_part_id(&part) {
        return Err(bad("bad_part_id"));
    }
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain; charset=utf-8");
    store.put_sidecar(&p.profile_id, "blob", &chat, &part, ct, &body)?;
    Ok(axum::Json(json!({"ok":true,"bytes":body.len()})).into_response())
}
async fn blob_get(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path((chat, part)): Path<(String, String)>,
) -> ApiResult {
    sidecar_response(store.get_sidecar(&p.profile_id, "blob", &chat, &part)?)
}
async fn blob_head(
    State(store): State<PeerStore>,
    Extension(p): Extension<PeerPrincipal>,
    Path((chat, part)): Path<(String, String)>,
) -> ApiResult {
    let Some((ct, b)) = store.get_sidecar(&p.profile_id, "blob", &chat, &part)? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            json!({"error":"not_found"}),
        ));
    };
    let mut r = StatusCode::OK.into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_str(&ct).unwrap());
    r.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&b.len().to_string()).unwrap(),
    );
    Ok(r)
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod revocation_tests {
    use super::*;

    fn connection(tx: mpsc::UnboundedSender<Message>) -> LiveConn {
        LiveConn {
            profile: "profile".into(),
            principal_device: "device".into(),
            room: "room".into(),
            kind: ConnKind::Chat,
            conn_id: String::new(),
            tx,
        }
    }

    #[test]
    fn revoke_before_late_registration_fails_closed() {
        let live = LiveState::default();
        live.revoke("profile", "device");
        let (tx, mut rx) = mpsc::unbounded_channel();
        live.add(connection(tx));
        assert_eq!(live.count("profile", "room", ConnKind::Chat), 0);
        assert!(matches!(rx.try_recv(), Ok(Message::Close(Some(close))) if close.code == 4403));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn revoke_removes_an_already_registered_connection() {
        let live = LiveState::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        live.add(connection(tx));
        assert_eq!(live.count("profile", "room", ConnKind::Chat), 1);
        live.revoke("profile", "device");
        assert_eq!(live.count("profile", "room", ConnKind::Chat), 0);
        assert!(matches!(rx.try_recv(), Ok(Message::Close(Some(close))) if close.code == 4403));
    }
}
