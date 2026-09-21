use std::path::Path;

use axum::{
    Router,
    body::Body,
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use base64::Engine as _;
use reqwest::StatusCode;
use tempfile::TempDir;
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

fn auth(request: reqwest::RequestBuilder, profile: &str, device: &str) -> reqwest::RequestBuilder {
    request
        .header("x-profile", profile)
        .header("x-device", device)
}

fn decode_pull(mut bytes: &[u8]) -> Vec<chat_frames::WireFrame> {
    let mut frames = Vec::new();
    while !bytes.is_empty() {
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        frames.push(chat_frames::decode(&bytes[4..4 + len]).unwrap());
        bytes = &bytes[4 + len..];
    }
    frames
}

#[tokio::test]
async fn chat_rows_dedupe_restart_range_and_profile_isolation() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("peer.sqlite");
    let client = reqwest::Client::new();
    let (url, store, task) = start(&db).await;

    let doc = loro::LoroDoc::new();
    doc.set_peer_id(1).unwrap();
    doc.get_map("data").insert("key", "value").unwrap();
    doc.commit();
    let update = doc.export(loro::ExportMode::all_updates()).unwrap();
    let snapshot = doc.export(loro::ExportMode::Snapshot).unwrap();
    let frontier_bytes = doc.oplog_vv().encode();
    for _ in 0..2 {
        let response = auth(
            client
                .post(format!("{url}/chat2/c1/rows?batchId=b1"))
                .body(update.clone()),
            "alice",
            "a1",
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ack: serde_json::Value = response.json().await.unwrap();
        assert_eq!(ack["seq"], 1);
    }
    let conflict = auth(
        client
            .post(format!("{url}/chat2/c1/rows?batchId=b1"))
            .body(b"different".to_vec()),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(conflict.status(), StatusCode::BAD_REQUEST);
    let device_conflict = auth(
        client
            .post(format!("{url}/chat2/c1/rows?batchId=b1"))
            .body(update.clone()),
        "alice",
        "a2",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(device_conflict.status(), StatusCode::BAD_REQUEST);
    let other = auth(
        client.get(format!("{url}/chat2/c1/rows?after=0")),
        "bob",
        "b1",
    )
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let frames = decode_pull(&other);
    assert_eq!(
        frames.iter().filter(|f| f.kind == frame_type::ROW).count(),
        0
    );

    let malformed = auth(
        client
            .post(format!("{url}/chat2/c1/checkpoint?seqCovered=1"))
            .header(
                "x-chat2-frontier",
                base64::engine::general_purpose::STANDARD.encode(&frontier_bytes),
            )
            .body(b"not a loro snapshot".to_vec()),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    let frontier = base64::engine::general_purpose::STANDARD.encode(&frontier_bytes);
    let cp = auth(
        client
            .post(format!("{url}/chat2/c1/checkpoint?seqCovered=1"))
            .header("x-chat2-frontier", frontier)
            .body(snapshot.clone()),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(cp.status(), StatusCode::OK);
    let regressive = loro::LoroDoc::new();
    regressive.set_peer_id(2).unwrap();
    regressive.get_map("data").insert("other", true).unwrap();
    regressive.commit();
    let regressive_snapshot = regressive.export(loro::ExportMode::Snapshot).unwrap();
    let regressive_frontier =
        base64::engine::general_purpose::STANDARD.encode(regressive.oplog_vv().encode());
    let replacement = auth(
        client
            .post(format!("{url}/chat2/c1/checkpoint?seqCovered=1"))
            .header("x-chat2-frontier", regressive_frontier)
            .body(regressive_snapshot),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(replacement.status(), StatusCode::BAD_REQUEST);
    let replay = auth(
        client
            .post(format!("{url}/chat2/c1/rows?batchId=b1"))
            .body(update),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    let replay_ack: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay_ack["seq"], 1);
    assert_eq!(replay_ack["dup"], true);
    let ranged = auth(
        client
            .get(format!("{url}/chat2/c1/checkpoint"))
            .header("range", "bytes=4-"),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        ranged.headers()["content-range"],
        format!("bytes 4-{}/{}", snapshot.len() - 1, snapshot.len())
    );
    assert_eq!(&ranged.bytes().await.unwrap()[..], &snapshot[4..]);

    let backup_path = tmp.path().join("backup.sqlite");
    store.backup_to(&backup_path).unwrap();
    // Exercise replacement as well as first publication, then verify that the
    // lower-level helper produced a complete standalone snapshot.
    store.backup_to(&backup_path).unwrap();
    let backup = rusqlite::Connection::open(&backup_path).unwrap();
    assert_eq!(
        backup
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let backed_up_checkpoint: Vec<u8> = backup
        .query_row(
            "SELECT bytes FROM chat_checkpoints WHERE profile='alice' AND chat='c1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backed_up_checkpoint, snapshot);
    task.abort();
    drop(store);
    let (url, _store, task) = start(&db).await;
    let pulled = auth(
        client.get(format!("{url}/chat2/c1/rows?after=0")),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let frames = decode_pull(&pulled);
    assert_eq!(frames[0].header["headSeq"], 1);
    assert_eq!(frames[0].header["seqFloor"], 1);
    let checkpoint = auth(
        client.get(format!("{url}/chat2/c1/checkpoint")),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(&checkpoint[..], &snapshot);
    task.abort();
}

#[tokio::test]
async fn checkpoint_rejects_snapshot_missing_a_dependent_covered_update() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let client = reqwest::Client::new();

    let doc = loro::LoroDoc::new();
    doc.set_peer_id(7).unwrap();
    doc.get_map("data").insert("first", 1).unwrap();
    doc.commit();
    let first_vv = doc.oplog_vv();
    let first_update = doc.export(loro::ExportMode::all_updates()).unwrap();
    let incomplete_snapshot = doc.export(loro::ExportMode::Snapshot).unwrap();

    doc.get_map("data").insert("second", 2).unwrap();
    doc.commit();
    let dependent_update = doc.export(loro::ExportMode::updates(&first_vv)).unwrap();

    for (batch, update) in [("first", first_update), ("second", dependent_update)] {
        let response = auth(
            client
                .post(format!("{url}/chat2/c1/rows?batchId={batch}"))
                .body(update),
            "alice",
            "a1",
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let checkpoint = auth(
        client
            .post(format!("{url}/chat2/c1/checkpoint?seqCovered=2"))
            .header(
                "x-chat2-frontier",
                base64::engine::general_purpose::STANDARD.encode(first_vv.encode()),
            )
            .body(incomplete_snapshot),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(checkpoint.status(), StatusCode::BAD_REQUEST);

    let pulled = auth(
        client.get(format!("{url}/chat2/c1/rows?after=0")),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let frames = decode_pull(&pulled);
    assert_eq!(frames[0].header["seqFloor"], 0);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.kind == frame_type::ROW)
            .count(),
        2
    );

    task.abort();
}

#[tokio::test]
async fn uncompacted_chat_log_is_bounded_without_breaking_dedupe() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let client = reqwest::Client::new();
    let row = vec![7_u8; 1024 * 1024];

    for index in 0..16 {
        let response = auth(
            client
                .post(format!("{url}/chat2/c1/rows?batchId=b{index}"))
                .body(row.clone()),
            "alice",
            "a1",
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let full = auth(
        client
            .post(format!("{url}/chat2/c1/rows?batchId=overflow"))
            .body(vec![8_u8; 1]),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(full.status(), StatusCode::INSUFFICIENT_STORAGE);

    // A retry consults the durable batch ledger before the capacity check.
    let replay: serde_json::Value = auth(
        client
            .post(format!("{url}/chat2/c1/rows?batchId=b0"))
            .body(row),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(replay["seq"], 1);
    assert_eq!(replay["dup"], true);
    task.abort();
}

#[tokio::test]
async fn blob_sidecars_preserve_content_type_and_profile_boundary() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let client = reqwest::Client::new();
    let part = "m1%23c1.diff";
    let put = auth(
        client
            .put(format!("{url}/blob/c1/{part}"))
            .header("content-type", "text/x-diff")
            .body("patch"),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    let get = auth(client.get(format!("{url}/blob/c1/{part}")), "alice", "a2")
        .send()
        .await
        .unwrap();
    assert_eq!(get.headers()["content-type"], "text/x-diff");
    assert_eq!(get.bytes().await.unwrap(), "patch");
    let isolated = auth(client.get(format!("{url}/blob/c1/{part}")), "bob", "b1")
        .send()
        .await
        .unwrap();
    assert_eq!(isolated.status(), StatusCode::NOT_FOUND);
    task.abort();
}

#[tokio::test]
async fn registry_lww_dedupe_full_resync_and_isolation() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let client = reqwest::Client::new();
    let push = |batch: &str, value: &str, hlc: &str| {
        serde_json::json!({
            "batch": batch,
            "ops": [{"kind":"chats","id":"c1","op":"upsert","set":{"title":value},"hlc":hlc}]
        })
    };
    let newer = "0000000000100-000000-dev-a";
    let older = "0000000000099-000999-dev-b";
    let first = auth(
        client
            .post(format!("{url}/registry/o1/push"))
            .json(&push("b1", "new", newer)),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let concurrent = auth(
        client
            .post(format!("{url}/registry/o1/push"))
            .json(&push("b2", "old", older)),
        "alice",
        "a2",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(concurrent.status(), StatusCode::OK);
    let replay: serde_json::Value = auth(
        client
            .post(format!("{url}/registry/o1/push"))
            .json(&push("b1", "different", newer)),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(replay["seq"], 1);

    let state: serde_json::Value = auth(
        client.get(format!("{url}/registry/o1/rows?since=99")),
        "alice",
        "a1",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(state["full"], true);
    assert_eq!(state["rows"][0]["fields"]["title"], "new");
    let isolated: serde_json::Value =
        auth(client.get(format!("{url}/registry/o1/rows")), "bob", "b1")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(isolated["rows"].as_array().unwrap().len(), 0);
    task.abort();
}
