//! End-to-end authenticated preview transport through the production peer router.

#[path = "../src/peer_auth.rs"]
mod peer_auth;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{WebSocketUpgrade, ws::Message as AxumMessage},
    middleware,
    response::Response,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use peer_auth::{AuthStore, DeviceIdentity, Principal};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use zeron_preview::{
    catalog::Catalog,
    discovery::Listener,
    mux::{self, BoxIo, Connector},
    peer::Peers,
    proxy::{self, Router as PreviewRouter},
    signaling::{self, Config, TokenSource},
};
use zeron_sync::peer::preview::PreviewState;

struct PeerServer {
    base: String,
    task: tokio::task::JoinHandle<()>,
    revocations: tokio::task::JoinHandle<()>,
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.task.abort();
        self.revocations.abort();
    }
}

async fn serve_peer(auth: AuthStore, previews: PreviewState) -> PeerServer {
    let protected = zeron_sync::peer::preview::router(previews.clone()).layer(
        middleware::from_fn_with_state(auth.clone(), peer_auth::require_peer_principal),
    );
    let app = peer_auth::router(auth.clone()).merge(protected);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut events = auth.subscribe_revocations();
    let revocations = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            previews.revoke_device(&event.profile_id, &event.device_id);
        }
    });
    PeerServer {
        base: format!("http://{address}"),
        task,
        revocations,
    }
}

fn token(store: &AuthStore, identity: &DeviceIdentity, principal: &Principal) -> String {
    let challenge = store
        .create_challenge(&principal.profile_id, &principal.device_id)
        .unwrap();
    store
        .authenticate_challenge(&identity.sign_challenge(&challenge).unwrap())
        .unwrap()
        .token
}

struct StaticToken(String);

#[async_trait::async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

struct Backend(Catalog);

#[async_trait::async_trait]
impl Connector for Backend {
    async fn connect(&self, service: &str) -> anyhow::Result<BoxIo> {
        let route = self
            .0
            .local_route(service)
            .ok_or_else(|| anyhow::anyhow!("service is not advertised"))?;
        Ok(Box::new(TcpStream::connect(route.listener.address).await?))
    }
}

async fn backend_ws(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            match message {
                AxumMessage::Text(_) | AxumMessage::Binary(_) => {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
                AxumMessage::Close(_) => break,
                _ => {}
            }
        }
    })
}

async fn start_backend() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/large", get(|| async { vec![42u8; 4 * 1024 * 1024] }))
        .route("/ws", get(backend_ws));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, task)
}

fn advertise(catalog: &Catalog, root: &str, address: std::net::SocketAddr, pid: u32) {
    catalog
        .replace_local(vec![(
            root.into(),
            Listener {
                pid,
                parent: 1,
                cwd: root.into(),
                args: vec!["node".into(), "vite".into()],
                started_at: pid as u64,
                address,
                zeron_owned: true,
            },
        )])
        .unwrap();
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("preview state did not converge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_peer_relays_large_http_websocket_and_revocation_with_profile_isolation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let temp = tempfile::tempdir().unwrap();
        let auth = AuthStore::open(temp.path().join("auth.sqlite")).unwrap();
        let owner_key = DeviceIdentity::load_or_create(temp.path().join("owner.key")).unwrap();
        let viewer_key = DeviceIdentity::load_or_create(temp.path().join("viewer.key")).unwrap();
        let outsider_key = DeviceIdentity::load_or_create(temp.path().join("outsider.key")).unwrap();
        let owner = auth
            .create_profile(&owner_key.public_key(), Some("owner"))
            .unwrap();
        let invite = auth
            .create_invite(&owner.profile_id, &owner.device_id)
            .unwrap();
        let viewer = auth
            .redeem_invite(&viewer_key.redeem_request(&invite, Some("viewer".into())).unwrap())
            .unwrap();
        let outsider = auth
            .create_profile(&outsider_key.public_key(), Some("outsider"))
            .unwrap();
        let owner_token = token(&auth, &owner_key, &owner);
        let viewer_token = token(&auth, &viewer_key, &viewer);
        let outsider_token = token(&auth, &outsider_key, &outsider);

        let preview_state = PreviewState::default();
        let peer = serve_peer(auth.clone(), preview_state).await;
        let stop = CancellationToken::new();
        let (host_address, host_backend_task) = start_backend().await;
        let (viewer_address, viewer_backend_task) = start_backend().await;
        let (outsider_address, outsider_backend_task) = start_backend().await;
        let host_catalog = Catalog::open(
            temp.path().join("host.json"),
            owner.device_id.clone(),
            "Host".into(),
        )
        .unwrap();
        let viewer_catalog = Catalog::open(
            temp.path().join("viewer.json"),
            viewer.device_id.clone(),
            "Viewer".into(),
        )
        .unwrap();
        let outsider_catalog = Catalog::open(
            temp.path().join("outsider.json"),
            outsider.device_id.clone(),
            "Outsider".into(),
        )
        .unwrap();
        advertise(&host_catalog, "/work/host-project", host_address, 101);
        advertise(&viewer_catalog, "/work/viewer-project", viewer_address, 102);
        advertise(&outsider_catalog, "/work/outsider-project", outsider_address, 103);

        let host_connector = Arc::new(Backend(host_catalog.clone()));
        let viewer_connector = Arc::new(Backend(viewer_catalog.clone()));
        let outsider_connector = Arc::new(Backend(outsider_catalog.clone()));
        let (host_peers, host_output) = Peers::new(
            owner.device_id.clone(),
            host_connector,
            stop.child_token(),
        );
        let (viewer_peers, viewer_output) = Peers::new(
            viewer.device_id.clone(),
            viewer_connector.clone(),
            stop.child_token(),
        );
        let (outsider_peers, outsider_output) = Peers::new(
            outsider.device_id.clone(),
            outsider_connector,
            stop.child_token(),
        );
        let config = |token: String| Config {
            edge_url: peer.base.clone(),
            org_id: "shared-org-name".into(),
            tokens: Arc::new(StaticToken(token)),
        };
        let host_signal = tokio::spawn(signaling::run(
            config(owner_token),
            host_catalog.clone(),
            host_peers,
            host_output,
            stop.child_token(),
        ));
        let viewer_signal = tokio::spawn(signaling::run(
            config(viewer_token.clone()),
            viewer_catalog.clone(),
            viewer_peers.clone(),
            viewer_output,
            stop.child_token(),
        ));
        let outsider_signal = tokio::spawn(signaling::run(
            config(outsider_token),
            outsider_catalog.clone(),
            outsider_peers,
            outsider_output,
            stop.child_token(),
        ));

        wait_for(|| {
            host_catalog
                .snapshot()
                .services
                .iter()
                .any(|service| service.device_id == viewer.device_id)
                && viewer_catalog
                    .snapshot()
                    .services
                    .iter()
                    .any(|service| service.device_id == owner.device_id)
        })
        .await;
        assert!(
            host_catalog
                .snapshot()
                .services
                .iter()
                .all(|service| service.device_id != outsider.device_id)
        );
        assert!(
            outsider_catalog
                .snapshot()
                .services
                .iter()
                .all(|service| service.device_id != owner.device_id && service.device_id != viewer.device_id)
        );

        let local = mux::local(viewer_connector, stop.child_token());
        let (proxy_port, proxy_tasks) = proxy::serve(
            PreviewRouter {
                catalog: viewer_catalog.clone(),
                local,
                peers: viewer_peers,
            },
            0,
            stop.child_token(),
        )
        .await
        .unwrap();
        viewer_catalog.set_proxy_status(proxy_port, None);
        let remote = viewer_catalog
            .snapshot()
            .services
            .into_iter()
            .find(|service| service.device_id == owner.device_id)
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve(&remote.hostname, ([127, 0, 0, 1], proxy_port).into())
            .build()
            .unwrap();
        let response = client
            .get(format!("{}/large", remote.url(proxy_port)))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.bytes().await.unwrap();
        assert_eq!(body.len(), 4 * 1024 * 1024);
        assert!(body.iter().all(|byte| *byte == 42));

        let mut request = format!("ws://{}:{proxy_port}/ws", remote.hostname)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", remote.url(proxy_port).parse().unwrap());
        let tcp = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, proxy_port))
            .await
            .unwrap();
        let (mut websocket, _) = tokio_tungstenite::client_async(request, tcp).await.unwrap();
        websocket
            .send(Message::Binary(vec![7; 256 * 1024]))
            .await
            .unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::Binary(vec![7; 256 * 1024])
        );

        auth.revoke_device(&owner, &viewer.device_id).unwrap();
        wait_for(|| {
            host_catalog
                .snapshot()
                .services
                .iter()
                .all(|service| service.device_id != viewer.device_id)
        })
        .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match websocket.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("revocation did not close the active preview stream");

        let mut unauthorized = format!(
            "{}/preview/shared-org-name/ws?device={}",
            peer.base.replace("http://", "ws://"),
            viewer.device_id
        )
        .into_client_request()
        .unwrap();
        unauthorized.headers_mut().insert(
            "authorization",
            format!("Bearer {viewer_token}").parse().unwrap(),
        );
        let error = tokio_tungstenite::connect_async(unauthorized)
            .await
            .unwrap_err();
        assert!(matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED));

        stop.cancel();
        for task in proxy_tasks {
            let _ = task.await;
        }
        let _ = host_signal.await;
        let _ = viewer_signal.await;
        let _ = outsider_signal.await;
        host_backend_task.abort();
        viewer_backend_task.abort();
        outsider_backend_task.abort();
    })
    .await
    .expect("authenticated preview integration stalled");
}
