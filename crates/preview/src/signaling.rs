//! Authenticated catalog and application transport over the Tailcat-carried
//! durable peer WebSocket. The public configuration shape is retained for the
//! engine: `edge_url` is now the loopback HTTP base exposed by its Tailcat
//! adapter and connected securely to the durable peer.
use crate::{
    catalog::Catalog,
    peer::{OutgoingFrame, Peers, decode_envelope, encode_envelope},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use kratos_proto::PreviewService;

#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Option<String>;
}

/// Connection details for the authenticated application peer.
pub struct Config {
    pub edge_url: String,
    pub org_id: String,
    pub tokens: Arc<dyn TokenSource>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Incoming {
    Catalog {
        device: String,
        services: Vec<PreviewService>,
    },
    Gone {
        device: String,
    },
}

pub async fn run(
    config: Config,
    catalog: Catalog,
    peers: Peers,
    mut outgoing: mpsc::Receiver<OutgoingFrame>,
    stop: CancellationToken,
) {
    loop {
        let connected = connect(&config, &catalog, &peers, &mut outgoing);
        tokio::select! {
            _ = stop.cancelled() => break,
            result = connected => if let Err(error) = result {
                tracing::debug!(%error, "preview peer disconnected");
            },
        }
        catalog.clear_remote();
        peers.clear().await;
        while outgoing.try_recv().is_ok() {} // frames from the old lease are stale
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(3)) => {},
        }
    }
    catalog.clear_remote();
    peers.clear().await;
}

async fn connect(
    config: &Config,
    catalog: &Catalog,
    peers: &Peers,
    outgoing: &mut mpsc::Receiver<OutgoingFrame>,
) -> anyhow::Result<()> {
    let token = config
        .tokens
        .token()
        .await
        .ok_or_else(|| anyhow::anyhow!("preview authentication unavailable"))?;
    let mut url = reqwest::Url::parse(&config.edge_url)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid preview peer URL"))?;
    url.set_path(&format!("/preview/{}/ws", config.org_id));
    url.query_pairs_mut()
        .clear()
        .append_pair("device", catalog.device_id());
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let mut websocket_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    websocket_config.max_message_size = Some(16 * 1024);
    websocket_config.max_frame_size = Some(16 * 1024);
    websocket_config.write_buffer_size = 32 * 1024;
    websocket_config.max_write_buffer_size = 128 * 1024;
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false),
    )
    .await??;
    let (mut write, mut read) = socket.split();
    let mut changes = catalog.subscribe();
    let mut advertised = catalog.local_services();
    write
        .send(Message::Text(
            serde_json::json!({"type":"catalog", "services":advertised})
                .to_string()
                .into(),
        ))
        .await?;
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    let mut last_message = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                anyhow::ensure!(last_message.elapsed() < Duration::from_secs(40), "preview peer lease expired");
                write.send(Message::Text("ping".into())).await?;
            }
            result = changes.changed() => {
                result?;
                let next = catalog.local_services();
                if next != advertised {
                    write.send(Message::Text(serde_json::json!({"type":"catalog", "services":next}).to_string().into())).await?;
                    advertised = next;
                }
            }
            Some(frame) = outgoing.recv() => {
                write.send(Message::Binary(encode_envelope(&frame.to, &frame.bytes)?.into())).await?;
            }
            message = read.next() => {
                let message = message.ok_or_else(|| anyhow::anyhow!("preview peer closed"))??;
                last_message = tokio::time::Instant::now();
                match message {
                    Message::Text(text) if text == "pong" => {},
                    Message::Text(text) => {
                        anyhow::ensure!(text.len() <= 1024 * 1024, "oversized preview catalog message");
                        match serde_json::from_str::<Incoming>(&text)? {
                            Incoming::Catalog { device, services } => catalog.set_remote(&device, services)?,
                            Incoming::Gone { device } => {
                                catalog.remove_remote(&device);
                                peers.remove(&device).await;
                            }
                        }
                    }
                    Message::Binary(bytes) => {
                        let (from, payload) = decode_envelope(&bytes)?;
                        peers.receive(from, payload.to_vec()).await?;
                    }
                    Message::Ping(bytes) => write.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => anyhow::bail!("preview peer closed"),
                    _ => {},
                }
            }
        }
    }
}
