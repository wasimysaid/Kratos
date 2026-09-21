use std::path::Path;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use futures::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use kratos_sync::chat_frames::{self, frame_type};
use kratos_sync::peer::{PeerPrincipal, PeerStore, router};

async fn principal(mut req: Request<Body>, next: Next) -> Response {
    let profile = req
        .headers()
        .get("x-profile")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let device = req
        .headers()
        .get("x-device")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    req.extensions_mut().insert(PeerPrincipal {
        profile_id: profile,
        device_id: device,
    });
    next.run(req).await
}
async fn start(path: &Path) -> (String, PeerStore, tokio::task::JoinHandle<()>) {
    let store = PeerStore::open(path).unwrap();
    let app: Router = router(store.clone()).layer(middleware::from_fn(principal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), store, task)
}
async fn ws(
    url: String,
    profile: &str,
    device: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = url
        .replace("http://", "ws://")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-profile", HeaderValue::from_str(profile).unwrap());
    request
        .headers_mut()
        .insert("x-device", HeaderValue::from_str(device).unwrap());
    connect_async(request).await.unwrap().0
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Header {
    s: String,
    k: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}
fn encode(h: &Header, payload: &[u8]) -> Vec<u8> {
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
fn decode(bytes: &[u8]) -> (Header, Vec<u8>) {
    let (mut n, mut shift, mut off) = (0usize, 0, 0);
    loop {
        let b = bytes[off];
        off += 1;
        n |= ((b & 127) as usize) << shift;
        if b & 128 == 0 {
            break;
        }
        shift += 7
    }
    let h = serde_json::from_slice(&bytes[off..off + n]).unwrap();
    (h, bytes[off + n..].to_vec())
}

#[tokio::test]
async fn chat_socket_relays_and_revoke_disconnects_only_principal_device() {
    let tmp = TempDir::new().unwrap();
    let (url, store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let mut a = ws(format!("{url}/chat2/c1/ws"), "p1", "a").await;
    let mut b = ws(format!("{url}/chat2/c1/ws"), "p1", "b").await;
    for socket in [&mut a, &mut b] {
        socket
            .send(Message::Binary(chat_frames::encode(
                frame_type::HELLO,
                &serde_json::json!({"cursor":0,"device":"impersonation-attempt"}),
                &[],
            )))
            .await
            .unwrap();
        let state = socket.next().await.unwrap().unwrap().into_data();
        assert_eq!(chat_frames::decode(&state).unwrap().kind, frame_type::STATE);
    }
    a.send(Message::Binary(chat_frames::encode(
        frame_type::PUSH,
        &serde_json::json!({"batchId":"batch-1"}),
        b"row",
    )))
    .await
    .unwrap();
    let ack = chat_frames::decode(&a.next().await.unwrap().unwrap().into_data()).unwrap();
    assert_eq!(ack.kind, frame_type::ACK);
    let row = chat_frames::decode(&b.next().await.unwrap().unwrap().into_data()).unwrap();
    assert_eq!(row.kind, frame_type::ROW);
    assert_eq!(row.header["device"], "a");
    store.revoke_device("p1", "a");
    let closed = tokio::time::timeout(Duration::from_secs(2), a.next())
        .await
        .unwrap();
    assert!(matches!(closed, Some(Ok(Message::Close(_))) | None));
    b.send(Message::Binary(chat_frames::encode(
        frame_type::PROBE,
        &serde_json::json!({}),
        &[],
    )))
    .await
    .unwrap();
    let probe = chat_frames::decode(&b.next().await.unwrap().unwrap().into_data()).unwrap();
    assert_eq!(probe.kind, frame_type::PROBE_OK);
    task.abort();
}

#[tokio::test]
async fn offline_nudge_survives_restart_and_device_relay_routes_both_ways() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("peer.sqlite");
    let client = reqwest::Client::new();
    let (url, store, task) = start(&db).await;
    let response = client
        .post(format!("{url}/device/host1/nudge"))
        .header("x-profile", "p1")
        .header("x-device", "remote")
        .json(&serde_json::json!({"chatId":"chat7"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["queued"],
        true
    );
    task.abort();
    drop(store);
    let (url, _store, task) = start(&db).await;
    let mut host = ws(format!("{url}/device/host1/ws?role=host"), "p1", "host1").await;
    let nudge = host.next().await.unwrap().unwrap().into_data();
    let (h, payload) = decode(&nudge);
    assert_eq!(h.k, "nudge");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["chatId"],
        "chat7"
    );
    let mut remote = ws(
        format!("{url}/device/host1/ws?role=client&connId=remote-conn"),
        "p1",
        "remote",
    )
    .await;
    remote
        .send(Message::Binary(encode(
            &Header {
                s: "s1".into(),
                k: "rpc".into(),
                to: None,
                from: None,
            },
            b"request",
        )))
        .await
        .unwrap();
    let (host_header, request) = decode(&host.next().await.unwrap().unwrap().into_data());
    assert_eq!(host_header.from.as_deref(), Some("remote-conn"));
    assert_eq!(request, b"request");
    host.send(Message::Binary(encode(
        &Header {
            s: "s1".into(),
            k: "rpc".into(),
            to: Some("remote-conn".into()),
            from: None,
        },
        b"reply",
    )))
    .await
    .unwrap();
    let (remote_header, reply) = decode(&remote.next().await.unwrap().unwrap().into_data());
    assert!(remote_header.to.is_none());
    assert!(remote_header.from.is_none());
    assert_eq!(reply, b"reply");
    task.abort();
}

#[tokio::test]
async fn registry_socket_sends_merged_rows_before_ack() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let mut socket = ws(format!("{url}/registry/o1/ws"), "p1", "a").await;
    socket
        .send(Message::Text(
            serde_json::json!({"t":"hello","cursor":null,"device":"spoofed"}).to_string(),
        ))
        .await
        .unwrap();
    let state: serde_json::Value =
        serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(state["t"], "state");
    socket
        .send(Message::Text(
            serde_json::json!({
                "t":"push",
                "batch":"rb1",
                "ops":[{"kind":"chats","id":"c1","op":"upsert","set":{"title":"hello"},"hlc":"0000000000100-000000-a"}]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let rows: serde_json::Value =
        serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let ack: serde_json::Value =
        serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(rows["t"], "rows");
    assert_eq!(ack["t"], "ack");
    assert_eq!(rows["rows"][0]["fields"]["title"], "hello");
    task.abort();
}

#[tokio::test]
async fn host_connection_cannot_impersonate_another_device() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let mut request = format!("{url}/device/victim/ws?role=host")
        .replace("http://", "ws://")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-profile", HeaderValue::from_static("p1"));
    request
        .headers_mut()
        .insert("x-device", HeaderValue::from_static("attacker"));
    let error = connect_async(request).await.unwrap_err();
    assert!(error.to_string().contains("403"));
    task.abort();
}
