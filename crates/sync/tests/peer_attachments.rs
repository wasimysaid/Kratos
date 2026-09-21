use std::path::Path;

use axum::{
    Router,
    body::Body,
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use reqwest::StatusCode;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use kratos_sync::peer::{PeerPrincipal, PeerStore, router};

async fn principal(mut req: Request<Body>, next: Next) -> Response {
    let profile = req.headers()["x-profile"].to_str().unwrap().to_owned();
    let device = req.headers()["x-device"].to_str().unwrap().to_owned();
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

#[tokio::test]
async fn custody_resumes_across_restart_and_target_reads_range() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("peer.sqlite");
    let bytes = b"durable attachment after sender exits";
    let digest = format!("{:x}", Sha256::digest(bytes));
    let client = reqwest::Client::new();
    let (url, store, task) = start(&db).await;
    let init = auth(
        client
            .post(format!("{url}/attachment/u1"))
            .json(&serde_json::json!({
                "targetDevice":"host","fileName":"photo.png","length":bytes.len(),"digest":digest
            })),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(init.status(), StatusCode::OK);
    let first = auth(
        client
            .put(format!(
                "{url}/attachment/u1/chunk?targetDevice=host&offset=0"
            ))
            .body(bytes[..10].to_vec()),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        first.json::<serde_json::Value>().await.unwrap()["nextOffset"],
        10
    );
    task.abort();
    drop(store);

    let (url, _store, task) = start(&db).await;
    let status: serde_json::Value = auth(
        client.get(format!(
            "{url}/attachment/u1/status?senderDevice=sender&targetDevice=host"
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
    assert_eq!(status["nextOffset"], 10);
    auth(
        client
            .put(format!(
                "{url}/attachment/u1/chunk?targetDevice=host&offset=10"
            ))
            .body(bytes[10..].to_vec()),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    auth(
        client.post(format!("{url}/attachment/u1/commit?targetDevice=host")),
        "profile-a",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let range = auth(
        client
            .get(format!(
                "{url}/attachment/u1?senderDevice=sender&targetDevice=host"
            ))
            .header("range", "bytes=8-"),
        "profile-a",
        "host",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.bytes().await.unwrap().as_ref(), &bytes[8..]);
    let wrong = auth(
        client.get(format!(
            "{url}/attachment/u1?senderDevice=sender&targetDevice=host"
        )),
        "profile-b",
        "host",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    task.abort();
}

#[tokio::test]
async fn commit_rejects_missing_chunks_and_digest_mismatch() {
    let tmp = TempDir::new().unwrap();
    let (url, _store, task) = start(&tmp.path().join("peer.sqlite")).await;
    let client = reqwest::Client::new();
    let init = |upload: &str, digest: String| {
        auth(
            client
                .post(format!("{url}/attachment/{upload}"))
                .json(&serde_json::json!({
                    "targetDevice":"host","fileName":"x.png","length":6,"digest":digest
                })),
            "p",
            "sender",
        )
    };
    init("missing", format!("{:x}", Sha256::digest(b"abcdef")))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    auth(
        client
            .put(format!(
                "{url}/attachment/missing/chunk?targetDevice=host&offset=3"
            ))
            .body("def"),
        "p",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let missing = auth(
        client.post(format!("{url}/attachment/missing/commit?targetDevice=host")),
        "p",
        "sender",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    init("digest", "0".repeat(64))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    auth(
        client
            .put(format!(
                "{url}/attachment/digest/chunk?targetDevice=host&offset=0"
            ))
            .body("abcdef"),
        "p",
        "sender",
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let mismatch = auth(
        client.post(format!("{url}/attachment/digest/commit?targetDevice=host")),
        "p",
        "sender",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    task.abort();
}
