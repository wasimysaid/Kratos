//! Managed Tailcat adapter and authenticated durable peer host runtime.

#[path = "peer_backup.rs"]
pub mod backup;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::middleware;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use kratos_sync::peer::PeerStore;

use crate::peer_auth::{AuthStore, require_peer_principal};

const ADAPTER_NAME: &str = if cfg!(windows) {
    "kratos-tailcat.exe"
} else {
    "kratos-tailcat"
};
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BACKUP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum PeerRuntimeError {
    #[error(
        "tailcat adapter was not found; expected it adjacent to the executable or set KRATOS_TAILCAT_ADAPTER"
    )]
    AdapterNotFound,
    #[error("tailcat adapter failed to start: {0}")]
    AdapterStart(#[source] std::io::Error),
    #[error("tailcat adapter readiness timed out")]
    ReadinessTimeout,
    #[error("tailcat adapter exited before readiness")]
    ReadinessEof,
    #[error("tailcat adapter returned malformed readiness")]
    InvalidReadiness,
    #[error("invalid Tailcat address")]
    InvalidAddress,
    #[error("invalid DERP map URL")]
    InvalidDerpMap,
    #[error("peer listener failed: {0}")]
    Listener(#[source] std::io::Error),
    #[error("peer store failed: {0}")]
    Store(#[from] kratos_sync::peer::PeerStoreError),
}

#[derive(Clone, Debug)]
pub struct PeerRuntimeConfig {
    pub data_dir: PathBuf,
    pub adapter_path: Option<PathBuf>,
    pub derp_map: Option<String>,
    pub backup_interval: Duration,

    /// Stable loopback port retained across adapter crash recovery. Zero lets
    /// the OS choose a port during first initialization only.
    pub local_port: u16,
}

impl PeerRuntimeConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            adapter_path: None,
            derp_map: None,
            backup_interval: DEFAULT_BACKUP_INTERVAL,

            local_port: 0,
        }
    }

    fn adapter_path(&self) -> Result<PathBuf, PeerRuntimeError> {
        if let Some(path) = &self.adapter_path {
            return regular_executable(path);
        }
        if let Some(path) = std::env::var_os("KRATOS_TAILCAT_ADAPTER") {
            return regular_executable(Path::new(&path));
        }
        let current = std::env::current_exe().map_err(PeerRuntimeError::AdapterStart)?;
        regular_executable(&current.with_file_name(ADAPTER_NAME))
    }

    fn validated_derp_map(&self) -> Result<Option<String>, PeerRuntimeError> {
        self.derp_map.as_deref().map(validate_derp_map).transpose()
    }
}

/// A running local Tailcat connector. Dropping it terminates the adapter.
pub struct PeerConnection {
    url: String,
    adapter: AdapterProcess,
}

impl PeerConnection {
    pub async fn connect(
        config: &PeerRuntimeConfig,
        address: &str,
    ) -> Result<Self, PeerRuntimeError> {
        validate_tailcat_address(address)?;
        let adapter_path = config.adapter_path()?;
        let derp_map = config.validated_derp_map()?;
        let peer_dir = config.data_dir.join("peer");
        let state = peer_dir.join("tailcat-client.key");
        create_private_dir(&peer_dir).map_err(PeerRuntimeError::AdapterStart)?;
        let connection_config =
            peer_dir.join(format!(".tailcat-connect-{}.json", uuid::Uuid::new_v4()));
        write_private_file(
            &connection_config,
            serde_json::json!({ "address": address })
                .to_string()
                .as_bytes(),
        )
        .map_err(PeerRuntimeError::AdapterStart)?;
        let mut args = vec![
            "connect".to_string(),
            "--config".to_string(),
            connection_config.to_string_lossy().into_owned(),
            "--listen".to_string(),
            format!("127.0.0.1:{}", config.local_port),
            "--state".to_string(),
            state.to_string_lossy().into_owned(),
        ];
        if let Some(derp_map) = derp_map {
            args.push("--derp-map".into());
            args.push(derp_map);
        }
        let started = AdapterProcess::start(&adapter_path, &args).await;
        let _ = std::fs::remove_file(&connection_config);
        let (adapter, readiness) = started?;
        let ready: ConnectReady =
            serde_json::from_str(&readiness).map_err(|_| PeerRuntimeError::InvalidReadiness)?;
        validate_loopback_url(&ready.url)?;
        Ok(Self {
            url: ready.url,
            adapter,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_running(&self) -> bool {
        self.adapter.is_running()
    }

    pub async fn shutdown(&mut self) {
        self.adapter.shutdown().await;
    }
}

/// The headless-capable durable peer: a loopback Axum listener behind one
/// managed Tailcat server, plus periodic consistent SQLite backups.
pub struct PeerRuntime {
    address: String,
    local_url: String,
    store: PeerStore,
    adapter: AdapterProcess,
    server_task: tokio::task::JoinHandle<()>,
    backup_task: tokio::task::JoinHandle<()>,
    revocation_task: tokio::task::JoinHandle<()>,
}

impl PeerRuntime {
    pub async fn host(
        config: &PeerRuntimeConfig,
        auth_store: AuthStore,
    ) -> Result<Self, PeerRuntimeError> {
        let peer_dir = config.data_dir.join("peer");
        create_private_dir(&peer_dir).map_err(PeerRuntimeError::AdapterStart)?;
        let store = PeerStore::open(peer_dir.join("peer.sqlite"))?;
        let previews = kratos_sync::peer::preview::PreviewState::default();
        let protected_routes = kratos_sync::peer::router(store.clone())
            .merge(kratos_sync::peer::preview::router(previews.clone()))
            .layer(middleware::from_fn_with_state(
                auth_store.clone(),
                require_peer_principal,
            ));
        let app = crate::peer_auth::router(auth_store.clone()).merge(protected_routes);
        // Resolve every fallible static input before publishing the listener.
        let adapter_path = config.adapter_path()?;
        let derp_map = config.validated_derp_map()?;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.local_port))
            .await
            .map_err(PeerRuntimeError::Listener)?;
        let socket = listener.local_addr().map_err(PeerRuntimeError::Listener)?;
        let local_url = format!("http://{socket}");
        let startup_server = StartupTaskGuard::new(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "authenticated peer listener stopped");
            }
        }));
        let state = peer_dir.join("tailcat-server.key");
        let mut args = vec![
            "serve".to_string(),
            "--state".to_string(),
            state.to_string_lossy().into_owned(),
            "--target".to_string(),
            format!("127.0.0.1:{}", socket.port()),
        ];
        if let Some(derp_map) = derp_map {
            args.push("--derp-map".into());
            args.push(derp_map);
        }
        let (adapter, readiness) = match AdapterProcess::start(&adapter_path, &args).await {
            Ok(value) => value,
            Err(error) => {
                startup_server.abort_and_wait().await;
                return Err(error);
            }
        };
        let ready: ServeReady = match serde_json::from_str(&readiness) {
            Ok(ready) => ready,
            Err(_) => {
                startup_server.abort_and_wait().await;
                return Err(PeerRuntimeError::InvalidReadiness);
            }
        };
        if let Err(error) = validate_tailcat_address(&ready.address) {
            startup_server.abort_and_wait().await;
            return Err(error);
        }
        let server_task = startup_server.disarm();

        let backup_store = store.clone();
        let backup_data_dir = config.data_dir.clone();
        let interval = config.backup_interval.max(Duration::from_secs(1));
        let backup_task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Initialization persists peer-session.json only after `host`
            // returns. Do not race the first complete generation against it.
            timer.tick().await;
            loop {
                timer.tick().await;
                let store = backup_store.clone();
                let data_dir = backup_data_dir.clone();
                match tokio::task::spawn_blocking(move || {
                    backup::create_generation(&data_dir, &store)
                })
                .await
                {
                    Ok(Ok(generation)) => {
                        tracing::info!(path = %generation.display(), "peer recovery generation published")
                    }
                    Ok(Err(error)) => {
                        eprintln!("peer recovery backup failed: {error}");
                        tracing::error!(%error, "peer recovery backup failed");
                    }
                    Err(error) => tracing::error!(%error, "peer recovery backup task failed"),
                }
            }
        });
        let revoke_store = store.clone();
        let revoke_previews = previews.clone();
        let mut revocations = auth_store.subscribe_revocations();
        let revocation_task = tokio::spawn(async move {
            let mut reconcile = tokio::time::interval(Duration::from_millis(100));
            reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let reconcile_all = tokio::select! {
                    event = revocations.recv() => match event {
                        Ok(event) => {
                            revoke_store.revoke_device(&event.profile_id, &event.device_id);
                            revoke_previews.revoke_device(&event.profile_id, &event.device_id);
                            false
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = reconcile.tick() => true,
                };
                if reconcile_all {
                    match auth_store.revoked_devices() {
                        Ok(events) => {
                            for event in events {
                                revoke_store.revoke_device(&event.profile_id, &event.device_id);
                                revoke_previews.revoke_device(&event.profile_id, &event.device_id);
                            }
                        }
                        Err(error) => tracing::error!(%error, "revocation reconciliation failed"),
                    }
                }
            }
        });

        Ok(Self {
            address: ready.address,
            local_url,
            store,
            adapter,
            server_task,
            backup_task,
            revocation_task,
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn local_url(&self) -> &str {
        &self.local_url
    }

    pub fn store(&self) -> &PeerStore {
        &self.store
    }

    pub fn is_running(&self) -> bool {
        self.adapter.is_running()
    }

    pub async fn shutdown(&mut self) {
        self.server_task.abort();
        self.backup_task.abort();
        self.revocation_task.abort();
        let _ = (&mut self.server_task).await;
        let _ = (&mut self.backup_task).await;
        let _ = (&mut self.revocation_task).await;
        self.adapter.shutdown().await;
    }
}

impl Drop for PeerRuntime {
    fn drop(&mut self) {
        self.server_task.abort();
        self.backup_task.abort();
        self.revocation_task.abort();
    }
}

struct StartupTaskGuard(Option<tokio::task::JoinHandle<()>>);

impl StartupTaskGuard {
    fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    async fn abort_and_wait(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }

    fn disarm(mut self) -> tokio::task::JoinHandle<()> {
        self.0.take().expect("startup server task")
    }
}

impl Drop for StartupTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

struct AdapterProcess {
    child: Mutex<Child>,
}

impl AdapterProcess {
    async fn start(path: &Path, args: &[String]) -> Result<(Self, String), PeerRuntimeError> {
        let mut command = Command::new(path);
        command
            .args(args)
            // The adapter exits when this pipe closes, even if AppKit/exit or
            // a crash bypasses Rust destructors. Child retains the write end;
            // it is never handed to a room task or inherited by another exec.
            .env("KRATOS_TAILCAT_PARENT_PIPE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(PeerRuntimeError::AdapterStart)?;
        let stdout = child.stdout.take().ok_or(PeerRuntimeError::ReadinessEof)?;
        let mut reader = BufReader::new(stdout);
        let mut readiness = String::new();
        let read = tokio::time::timeout(READY_TIMEOUT, reader.read_line(&mut readiness))
            .await
            .map_err(|_| PeerRuntimeError::ReadinessTimeout)?
            .map_err(PeerRuntimeError::AdapterStart)?;
        if read == 0 {
            return Err(PeerRuntimeError::ReadinessEof);
        }
        if readiness.len() > 4096 || !readiness.ends_with('\n') {
            return Err(PeerRuntimeError::InvalidReadiness);
        }
        Ok((
            Self {
                child: Mutex::new(child),
            },
            readiness,
        ))
    }

    fn is_running(&self) -> bool {
        self.child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_wait()
            .ok()
            .flatten()
            .is_none()
    }

    async fn shutdown(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl Drop for AdapterProcess {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = child.start_kill();
    }
}

#[derive(Deserialize)]
struct ConnectReady {
    url: String,
}

#[derive(Deserialize)]
struct ServeReady {
    address: String,
}

pub fn validate_tailcat_address(address: &str) -> Result<(), PeerRuntimeError> {
    if address.len() < 16
        || address.len() > 512
        || !address.starts_with("tc")
        || !address.is_ascii()
        || address.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(PeerRuntimeError::InvalidAddress);
    }
    Ok(())
}

pub fn validate_derp_map(value: &str) -> Result<String, PeerRuntimeError> {
    let url = reqwest::Url::parse(value).map_err(|_| PeerRuntimeError::InvalidDerpMap)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(PeerRuntimeError::InvalidDerpMap);
    }
    Ok(url.to_string())
}

fn validate_loopback_url(value: &str) -> Result<(), PeerRuntimeError> {
    let url = reqwest::Url::parse(value).map_err(|_| PeerRuntimeError::InvalidReadiness)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PeerRuntimeError::InvalidReadiness);
    }
    Ok(())
}

fn regular_executable(path: &Path) -> Result<PathBuf, PeerRuntimeError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PeerRuntimeError::AdapterNotFound
        } else {
            PeerRuntimeError::AdapterStart(error)
        }
    })?;
    if !metadata.is_file() {
        return Err(PeerRuntimeError::AdapterNotFound);
    }
    Ok(path.to_path_buf())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
