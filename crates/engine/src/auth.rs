//! Durable profile and trusted-device authentication over a managed Tailcat peer.
//!
//! A local installation has no implicit cloud identity. It remains signed out
//! until it initializes a peer or redeems an explicit invitation. Persisted
//! sessions contain only an opaque profile UUID, the authorized device id, and
//! peer connectivity settings; legacy WorkOS session files are never read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::EngineError;
use crate::peer_auth::{
    AuthStore, Challenge, ChallengeRequest, DeviceIdentity, DeviceRecord, Invite, Principal,
    TokenResponse,
};
use crate::peer_runtime::{
    PeerConnection, PeerRuntime, PeerRuntimeConfig, validate_derp_map, validate_tailcat_address,
};

const SESSION_VERSION: u8 = 1;
const INVITATION_VERSION: u8 = 1;
const SESSION_FILE: &str = "peer-session.json";
const IDENTITY_FILE: &str = "peer-device.json";
const TOKEN_REFRESH_SLACK: i64 = 30;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const INVITATION_PREFIX: &str = "kratos-pair:";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Transitional frontend wire shape. Pairing has no organization onboarding;
/// `orgId` is always absent and the user id is the opaque auth profile UUID.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    SignedIn {
        user: AuthUser,
        org_id: Option<String>,
    },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }

    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            Self::SignedIn { user, .. } => Some(user),
            Self::SignedOut => None,
        }
    }

    pub fn org_id(&self) -> Option<&str> {
        None
    }

    pub fn to_proto(&self) -> kratos_proto::AuthState {
        match self {
            Self::SignedOut => kratos_proto::AuthState::SignedOut,
            Self::SignedIn { user, .. } => kratos_proto::AuthState::SignedIn {
                user: kratos_proto::UserProfile {
                    id: user.id.clone(),
                    email: user.email.clone(),
                    name: user.name.clone(),
                },
                org_id: None,
            },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub data_dir: PathBuf,
    pub adapter_path: Option<PathBuf>,
    pub backup_interval: Duration,
}

impl AuthConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            adapter_path: None,
            backup_interval: Duration::from_secs(6 * 60 * 60),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingInvitation {
    pub version: u8,
    pub address: String,
    pub invite: Invite,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derp_map: Option<String>,
}

impl PairingInvitation {
    pub fn encode(&self) -> Result<String, EngineError> {
        self.validate()?;
        let json = serde_json::to_vec(self)
            .map_err(|error| EngineError::Other(format!("serialize invitation: {error}")))?;
        Ok(format!(
            "{INVITATION_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        ))
    }

    /// Accept the compact paste code or raw JSON for CLI/debug input.
    pub fn parse(value: &str) -> Result<Self, EngineError> {
        let trimmed = value.trim();
        let bytes = if let Some(encoded) = trimmed.strip_prefix(INVITATION_PREFIX) {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| EngineError::Other("invalid pairing invitation".into()))?
        } else {
            trimmed.as_bytes().to_vec()
        };
        if bytes.len() > 16 * 1024 {
            return Err(EngineError::Other("pairing invitation is too large".into()));
        }
        let invitation: Self = serde_json::from_slice(&bytes)
            .map_err(|_| EngineError::Other("invalid pairing invitation".into()))?;
        invitation.validate()?;
        Ok(invitation)
    }

    fn validate(&self) -> Result<(), EngineError> {
        if self.version != INVITATION_VERSION
            || self.invite.version != INVITATION_VERSION
            || self.invite.profile_id.is_empty()
        {
            return Err(EngineError::Other("unsupported pairing invitation".into()));
        }
        validate_tailcat_address(&self.address)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        if let Some(url) = &self.derp_map {
            validate_derp_map(url).map_err(|error| EngineError::Other(error.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatus {
    pub signed_in: bool,
    pub hosting: bool,
    pub connected: bool,
    pub profile_id: Option<String>,
    pub device_id: Option<String>,
    /// Internal only. Tailcat addresses are bearer-like secrets and are never serialized.
    #[serde(skip_serializing)]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derp_map: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSession {
    version: u8,
    profile_id: String,
    device_id: String,
    address: String,
    hosting: bool,

    local_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    derp_map: Option<String>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: i64,
}

enum ActivePeer {
    Host(PeerRuntime),
    Client(PeerConnection),
}

impl ActivePeer {
    fn url(&self) -> &str {
        match self {
            Self::Host(runtime) => runtime.local_url(),
            Self::Client(connection) => connection.url(),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            Self::Host(runtime) => runtime.is_running(),
            Self::Client(connection) => connection.is_running(),
        }
    }

    async fn shutdown(mut self) {
        match &mut self {
            Self::Host(runtime) => runtime.shutdown().await,
            Self::Client(connection) => connection.shutdown().await,
        }
    }
}

struct AuthInner {
    config: AuthConfig,
    identity: Mutex<Arc<DeviceIdentity>>,
    store: AuthStore,
    http: reqwest::Client,
    session: Mutex<Option<StoredSession>>,
    token: Mutex<Option<CachedToken>>,
    active: Mutex<Option<ActivePeer>>,

    transport_suspended: AtomicBool,
    last_error: Mutex<Option<String>>,
    state_tx: watch::Sender<AuthState>,
    token_tx: watch::Sender<u64>,
    transition_gate: tokio::sync::Mutex<()>,
    transition_epoch: AtomicU64,
    runtime_gate: tokio::sync::Mutex<()>,
    refresh_gate: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct Auth {
    inner: Arc<AuthInner>,
}

impl Auth {
    pub fn open(config: AuthConfig) -> Result<Self, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let identity = Arc::new(
            DeviceIdentity::load_or_create(config.data_dir.join(IDENTITY_FILE))
                .map_err(|error| EngineError::Other(error.to_string()))?,
        );
        let peer_dir = config.data_dir.join("peer");
        std::fs::create_dir_all(&peer_dir)?;
        let store = AuthStore::open(peer_dir.join("auth.sqlite"))
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let session = read_session(&config.data_dir.join(SESSION_FILE))?;
        if let Some(session) = &session {
            validate_session(session)?;
        }
        let initial = session
            .as_ref()
            .map(auth_state)
            .unwrap_or(AuthState::SignedOut);
        let (state_tx, _) = watch::channel(initial);
        let (token_tx, _) = watch::channel(0);
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| EngineError::Other(format!("build peer HTTP client: {error}")))?;
        Ok(Self {
            inner: Arc::new(AuthInner {
                config,
                identity: Mutex::new(identity),
                store,
                http,
                session: Mutex::new(session),
                token: Mutex::new(None),
                active: Mutex::new(None),

                transport_suspended: AtomicBool::new(false),
                last_error: Mutex::new(None),
                state_tx,
                token_tx,
                transition_gate: tokio::sync::Mutex::new(()),
                transition_epoch: AtomicU64::new(0),
                runtime_gate: tokio::sync::Mutex::new(()),
                refresh_gate: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Resume a saved host/client adapter. Connectivity failure is observable
    /// through `peer_status` but never mutates the durable signed-in profile.
    pub async fn resume(&self) {
        // Shutdown is terminal for this Auth instance. A detached startup task
        // may reach resume after Quit; only a newly opened Auth may restart it.
        if self.inner.transport_suspended.load(Ordering::Acquire) {
            return;
        }
        if self.session().is_none() {
            return;
        }
        if let Err(error) = self.ensure_active().await {
            self.record_error(error.to_string());
            return;
        }
        let _ = self.refresh_token().await;
    }

    pub fn watch_state(&self) -> watch::Receiver<AuthState> {
        self.inner.state_tx.subscribe()
    }

    pub fn state(&self) -> AuthState {
        self.inner.state_tx.borrow().clone()
    }

    pub fn profile_id(&self) -> Option<String> {
        self.session().map(|session| session.profile_id)
    }

    /// Read the authenticated device identity persisted for a previously paired
    /// profile without starting Tailcat or performing any network I/O.
    pub(crate) fn persisted_device_id(data_dir: &Path) -> Result<Option<String>, EngineError> {
        let session = read_session(&data_dir.join(SESSION_FILE))?;
        if let Some(session) = &session {
            validate_session(session)?;
        }
        Ok(session.map(|session| session.device_id))
    }

    /// Authenticated sync/relay identity. This is independent of the
    /// installation-local `device-id` used by local-only runtimes.
    pub fn device_id(&self) -> Option<String> {
        self.session().map(|session| session.device_id)
    }

    pub fn peer_url(&self) -> Option<String> {
        lock(&self.inner.active)
            .as_ref()
            .filter(|active| active.is_running())
            .map(|active| active.url().to_string())
    }

    /// Stable loopback endpoint reserved in the durable session. Protocol
    /// supervisors may start against it while Tailcat is offline; authentication
    /// retries will bring the adapter up in the background without rebuilding
    /// profile-scoped stores.
    pub fn peer_endpoint(&self) -> Option<String> {
        self.session()
            .map(|session| format!("http://127.0.0.1:{}", session.local_port))
    }

    pub async fn initialize_peer(
        &self,
        name: &str,
        derp_map: Option<String>,
    ) -> Result<PeerStatus, EngineError> {
        let transition = self.inner.transition_gate.lock().await;
        let transition_epoch = self.inner.transition_epoch.load(Ordering::Acquire);
        if self.session().is_some() {
            return Err(EngineError::Other(
                "a peer profile is already configured".into(),
            ));
        }
        let derp_map = derp_map
            .as_deref()
            .map(validate_derp_map)
            .transpose()
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let local_port = reserve_loopback_port().await?;
        let runtime_config = self.runtime_config(derp_map.clone(), local_port);
        let runtime = PeerRuntime::host(&runtime_config, self.inner.store.clone())
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let principal = self
            .inner
            .store
            .create_profile(
                &self.identity().public_key(),
                (!name.trim().is_empty()).then_some(name.trim()),
            )
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let session = StoredSession {
            version: SESSION_VERSION,
            profile_id: principal.profile_id,
            device_id: principal.device_id,
            address: runtime.address().to_string(),
            hosting: true,

            local_port,
            derp_map,
        };
        self.install_session(session, ActivePeer::Host(runtime), None, transition_epoch)?;

        drop(transition);
        self.refresh_token().await?;
        Ok(self.peer_status().await)
    }

    pub async fn pair(&self, code: &str) -> Result<(), EngineError> {
        let transition = self.inner.transition_gate.lock().await;
        let transition_epoch = self.inner.transition_epoch.load(Ordering::Acquire);
        if self.session().is_some() {
            return Err(EngineError::Other(
                "a peer profile is already configured".into(),
            ));
        }
        let invitation = PairingInvitation::parse(code)?;
        let local_port = reserve_loopback_port().await?;
        let runtime_config = self.runtime_config(invitation.derp_map.clone(), local_port);
        let connection = PeerConnection::connect(&runtime_config, &invitation.address)
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let url = connection.url().to_string();
        let mut pairing_identity = self.identity();
        let mut replacing_identity = false;
        let mut proof = pairing_identity
            .redeem_request(&invitation.invite, None)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let mut response = self
            .inner
            .http
            .post(format!("{url}/pair/redeem"))
            .json(&proof)
            .send()
            .await
            .map_err(peer_http_error)?;
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            // Revoked public keys stay revoked. Stage a fresh credential in
            // memory, redeem the still-unused invitation with it, and publish
            // the key only after the peer accepts it.
            pairing_identity = Arc::new(DeviceIdentity::generate());
            replacing_identity = true;
            proof = pairing_identity
                .redeem_request(&invitation.invite, None)
                .map_err(|error| EngineError::Other(error.to_string()))?;
            response = self
                .inner
                .http
                .post(format!("{url}/pair/redeem"))
                .json(&proof)
                .send()
                .await
                .map_err(peer_http_error)?;
        }
        if !response.status().is_success() {
            return Err(http_status_error("pair invitation", response.status()));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Redeemed {
            profile_id: String,
            device_id: String,
        }
        let redeemed: Redeemed = response
            .json()
            .await
            .map_err(|_| EngineError::Other("peer returned malformed pairing response".into()))?;
        if redeemed.profile_id != invitation.invite.profile_id
            || redeemed.device_id != pairing_identity.device_id()
        {
            return Err(EngineError::Other(
                "peer returned a mismatched device identity".into(),
            ));
        }
        let replacement_identity = replacing_identity.then_some(pairing_identity);
        let session = StoredSession {
            version: SESSION_VERSION,
            profile_id: redeemed.profile_id,
            device_id: redeemed.device_id,
            address: invitation.address,
            hosting: false,

            local_port,
            derp_map: invitation.derp_map,
        };
        self.install_session(
            session,
            ActivePeer::Client(connection),
            replacement_identity,
            transition_epoch,
        )?;

        drop(transition);
        self.refresh_token().await?;
        Ok(())
    }

    pub async fn invite(&self) -> Result<String, EngineError> {
        let session = self.require_session()?;
        let token = self
            .fresh_token()
            .await?
            .ok_or_else(|| EngineError::Other("peer is offline".into()))?;
        let url = self.require_peer_url()?;
        let response = self
            .inner
            .http
            .post(format!("{url}/pair/invite"))
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(peer_http_error)?;
        if !response.status().is_success() {
            return Err(http_status_error("create invitation", response.status()));
        }
        let invite: Invite = response
            .json()
            .await
            .map_err(|_| EngineError::Other("peer returned malformed invitation".into()))?;
        PairingInvitation {
            version: INVITATION_VERSION,
            address: session.address,
            invite,
            derp_map: session.derp_map,
        }
        .encode()
    }

    pub async fn devices(&self) -> Result<Vec<DeviceRecord>, EngineError> {
        let token = self
            .fresh_token()
            .await?
            .ok_or_else(|| EngineError::Other("peer is offline".into()))?;
        let url = self.require_peer_url()?;
        let response = self
            .inner
            .http
            .get(format!("{url}/pair/devices"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(peer_http_error)?;
        if !response.status().is_success() {
            return Err(http_status_error("list devices", response.status()));
        }
        response
            .json()
            .await
            .map_err(|_| EngineError::Other("peer returned malformed device list".into()))
    }

    pub async fn revoke(&self, device_id: &str) -> Result<(), EngineError> {
        let own_device = self.device_id();
        let token = self
            .fresh_token()
            .await?
            .ok_or_else(|| EngineError::Other("peer is offline".into()))?;
        let url = self.require_peer_url()?;
        let response = self
            .inner
            .http
            .post(format!("{url}/pair/revoke"))
            .bearer_auth(token)
            .json(&serde_json::json!({ "deviceId": device_id }))
            .send()
            .await
            .map_err(peer_http_error)?;
        if !response.status().is_success() {
            return Err(http_status_error("revoke device", response.status()));
        }
        if own_device.as_deref() == Some(device_id) {
            self.sign_out();
        }
        Ok(())
    }

    pub async fn peer_status(&self) -> PeerStatus {
        let session = self.session();
        let connected = lock(&self.inner.active)
            .as_ref()
            .is_some_and(ActivePeer::is_running);
        PeerStatus {
            signed_in: session.is_some(),
            hosting: session.as_ref().is_some_and(|session| session.hosting),
            connected,
            profile_id: session.as_ref().map(|session| session.profile_id.clone()),
            device_id: session.as_ref().map(|session| session.device_id.clone()),
            address: session.as_ref().map(|session| session.address.clone()),
            derp_map: session.and_then(|session| session.derp_map),
            last_error: lock(&self.inner.last_error).clone(),
        }
    }

    /// Stop the managed peer transport without deleting the durable signed-in
    /// session or device identity. App/runtime replacement uses this boundary
    /// when it must prove that no old adapter or host listener remains alive.
    pub async fn shutdown_peer_preserving_session(&self) {
        self.inner
            .transport_suspended
            .store(true, Ordering::Release);
        // Serialize with ensure_active so a late room/token supervisor cannot
        // publish a replacement adapter after this shutdown boundary.
        let _gate = self.inner.runtime_gate.lock().await;
        *lock(&self.inner.token) = None;
        let active = lock(&self.inner.active).take();
        if let Some(active) = active {
            active.shutdown().await;
        }
        self.bump_tokens();
    }

    pub fn sign_out(&self) {
        // This synchronous API cannot wait on the async transition gate. Advancing
        // the epoch cancels an in-flight initialize/pair before publication, while
        // the session mutex orders sign-out against a publication already underway.
        self.inner.transition_epoch.fetch_add(1, Ordering::AcqRel);
        let mut session = lock(&self.inner.session);
        if let Err(error) = self.persist_session(None) {
            self.record_error(error.to_string());
        }
        *session = None;
        *lock(&self.inner.token) = None;
        lock(&self.inner.active).take();
        self.inner.state_tx.send_replace(AuthState::SignedOut);
        self.bump_tokens();
    }

    pub async fn access_token(&self) -> Option<String> {
        match self.fresh_token().await {
            Ok(token) => token,
            Err(error) => {
                self.record_error(error.to_string());
                None
            }
        }
    }

    pub fn spawn_refresh_loop(&self) -> tokio::task::JoinHandle<()> {
        let auth = self.clone();
        tokio::spawn(async move {
            let mut state = auth.watch_state();
            loop {
                if !state.borrow().is_signed_in() {
                    if state.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {
                        let _ = auth.fresh_token().await;
                    }
                    changed = state.changed() => if changed.is_err() { break; },
                }
            }
        })
    }

    async fn fresh_token(&self) -> Result<Option<String>, EngineError> {
        if self.session().is_none() {
            return Ok(None);
        }
        self.ensure_active().await?;
        if let Some(token) = lock(&self.inner.token).clone()
            && token.expires_at > unix_now().saturating_add(TOKEN_REFRESH_SLACK)
        {
            return Ok(Some(token.value));
        }
        self.refresh_token().await.map(Some)
    }

    async fn refresh_token(&self) -> Result<String, EngineError> {
        let _gate = self.inner.refresh_gate.lock().await;
        self.ensure_active().await?;
        if let Some(token) = lock(&self.inner.token).clone()
            && token.expires_at > unix_now().saturating_add(TOKEN_REFRESH_SLACK)
        {
            return Ok(token.value);
        }
        let session = self.require_session()?;
        let url = self.require_peer_url()?;
        let challenge_response = self
            .inner
            .http
            .post(format!("{url}/pair/challenge"))
            .json(&ChallengeRequest {
                profile_id: session.profile_id.clone(),
                device_id: session.device_id.clone(),
            })
            .send()
            .await
            .map_err(peer_http_error)?;
        if matches!(
            challenge_response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            self.sign_out();
            return Err(EngineError::Other(
                "device authorization was revoked".into(),
            ));
        }
        if !challenge_response.status().is_success() {
            return Err(http_status_error(
                "request authentication challenge",
                challenge_response.status(),
            ));
        }
        let challenge: Challenge = challenge_response
            .json()
            .await
            .map_err(|_| EngineError::Other("peer returned malformed challenge".into()))?;
        let proof = self
            .identity()
            .sign_challenge(&challenge)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let auth_response = self
            .inner
            .http
            .post(format!("{url}/pair/authenticate"))
            .json(&proof)
            .send()
            .await
            .map_err(peer_http_error)?;
        if matches!(
            auth_response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            self.sign_out();
            return Err(EngineError::Other(
                "device authorization was revoked".into(),
            ));
        }
        if !auth_response.status().is_success() {
            return Err(http_status_error(
                "authenticate device",
                auth_response.status(),
            ));
        }
        let token: TokenResponse = auth_response
            .json()
            .await
            .map_err(|_| EngineError::Other("peer returned malformed token".into()))?;
        if token.principal
            != (Principal {
                profile_id: session.profile_id,
                device_id: session.device_id,
            })
        {
            return Err(EngineError::Other("peer token principal mismatch".into()));
        }
        *lock(&self.inner.token) = Some(CachedToken {
            value: token.token.clone(),
            expires_at: token.expires_at,
        });
        *lock(&self.inner.last_error) = None;
        self.bump_tokens();

        // Adapter recovery is also network-path recovery for every room
        // supervisor. Wake all parked backoffs immediately; token watchers
        // alone can miss a refresh completed before their first failed dial.
        kratos_sync::wake::set_path_online(true);
        Ok(token.token)
    }

    async fn ensure_active(&self) -> Result<(), EngineError> {
        if self.inner.transport_suspended.load(Ordering::Acquire) {
            return Err(EngineError::Other("peer transport is suspended".into()));
        }
        if lock(&self.inner.active)
            .as_ref()
            .is_some_and(ActivePeer::is_running)
        {
            return Ok(());
        }
        let _gate = self.inner.runtime_gate.lock().await;

        if self.inner.transport_suspended.load(Ordering::Acquire) {
            return Err(EngineError::Other("peer transport is suspended".into()));
        }
        if lock(&self.inner.active)
            .as_ref()
            .is_some_and(ActivePeer::is_running)
        {
            return Ok(());
        }
        lock(&self.inner.active).take();
        let session = self.require_session()?;
        let config = self.runtime_config(session.derp_map.clone(), session.local_port);
        let active = if session.hosting {
            ActivePeer::Host(
                PeerRuntime::host(&config, self.inner.store.clone())
                    .await
                    .map_err(|error| EngineError::Other(error.to_string()))?,
            )
        } else {
            ActivePeer::Client(
                PeerConnection::connect(&config, &session.address)
                    .await
                    .map_err(|error| EngineError::Other(error.to_string()))?,
            )
        };
        if let ActivePeer::Host(runtime) = &active
            && runtime.address() != session.address
        {
            return Err(EngineError::Other(
                "persisted Tailcat host address changed unexpectedly".into(),
            ));
        }
        *lock(&self.inner.active) = Some(active);
        Ok(())
    }

    fn install_session(
        &self,
        session: StoredSession,
        active: ActivePeer,
        replacement_identity: Option<Arc<DeviceIdentity>>,
        transition_epoch: u64,
    ) -> Result<(), EngineError> {
        validate_session(&session)?;
        let mut session_slot = lock(&self.inner.session);
        if self.inner.transition_epoch.load(Ordering::Acquire) != transition_epoch {
            return Err(EngineError::Other(
                "authentication transition was cancelled".into(),
            ));
        }
        if let Some(identity) = replacement_identity {
            identity
                .persist(self.inner.config.data_dir.join(IDENTITY_FILE))
                .map_err(|error| EngineError::Other(error.to_string()))?;
            *lock(&self.inner.identity) = identity;
        }
        self.persist_session(Some(&session))?;
        *lock(&self.inner.active) = Some(active);
        *session_slot = Some(session.clone());
        *lock(&self.inner.token) = None;
        *lock(&self.inner.last_error) = None;
        self.inner.state_tx.send_replace(auth_state(&session));
        self.bump_tokens();
        Ok(())
    }

    fn runtime_config(&self, derp_map: Option<String>, local_port: u16) -> PeerRuntimeConfig {
        let mut config = PeerRuntimeConfig::new(&self.inner.config.data_dir);
        config.adapter_path = self.inner.config.adapter_path.clone();
        config.backup_interval = self.inner.config.backup_interval;
        config.derp_map = derp_map;
        config.local_port = local_port;
        config
    }

    fn identity(&self) -> Arc<DeviceIdentity> {
        lock(&self.inner.identity).clone()
    }

    fn session(&self) -> Option<StoredSession> {
        lock(&self.inner.session).clone()
    }

    fn require_session(&self) -> Result<StoredSession, EngineError> {
        self.session()
            .ok_or_else(|| EngineError::Other("peer is not initialized or paired".into()))
    }

    fn require_peer_url(&self) -> Result<String, EngineError> {
        self.peer_url()
            .ok_or_else(|| EngineError::Other("peer adapter is not connected".into()))
    }

    fn persist_session(&self, session: Option<&StoredSession>) -> Result<(), EngineError> {
        let path = self.inner.config.data_dir.join(SESSION_FILE);
        match session {
            Some(session) => {
                let mut bytes = serde_json::to_vec_pretty(session).map_err(|error| {
                    EngineError::Other(format!("serialize peer session: {error}"))
                })?;
                bytes.push(b'\n');
                atomic_private_write(&path, &bytes)?;
            }
            None => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
        Ok(())
    }

    fn record_error(&self, error: String) {
        *lock(&self.inner.last_error) = Some(error);
    }

    fn bump_tokens(&self) {
        let next = self.inner.token_tx.borrow().wrapping_add(1);
        self.inner.token_tx.send_replace(next);
    }
}

#[async_trait::async_trait]
impl kratos_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        self.access_token().await
    }

    fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        Some(self.inner.token_tx.subscribe())
    }
}

#[async_trait::async_trait]
impl kratos_preview::signaling::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        self.access_token().await
    }
}

fn auth_state(session: &StoredSession) -> AuthState {
    AuthState::SignedIn {
        user: AuthUser {
            id: session.profile_id.clone(),
            email: String::new(),
            name: None,
        },
        org_id: None,
    }
}

fn validate_session(session: &StoredSession) -> Result<(), EngineError> {
    if session.version != SESSION_VERSION
        || uuid::Uuid::parse_str(&session.profile_id)
            .ok()
            .is_none_or(|id| id.to_string() != session.profile_id)
        || !valid_peer_device_id(&session.device_id)
        || session.local_port == 0
    {
        return Err(EngineError::Other("invalid persisted peer session".into()));
    }
    validate_tailcat_address(&session.address)
        .map_err(|error| EngineError::Other(error.to_string()))?;
    if let Some(derp_map) = &session.derp_map {
        validate_derp_map(derp_map).map_err(|error| EngineError::Other(error.to_string()))?;
    }
    Ok(())
}
fn valid_peer_device_id(value: &str) -> bool {
    value.strip_prefix("dev_").is_some_and(|encoded| {
        encoded.len() == 43
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

fn read_session(path: &Path) -> Result<Option<StoredSession>, EngineError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    set_private_permissions(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| EngineError::Other(format!("invalid peer session: {error}")))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(SESSION_FILE),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<(), EngineError> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        set_private_permissions(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn peer_http_error(error: reqwest::Error) -> EngineError {
    EngineError::Other(if error.is_timeout() {
        "peer request timed out".into()
    } else {
        "peer is unavailable".into()
    })
}

fn http_status_error(operation: &str, status: reqwest::StatusCode) -> EngineError {
    EngineError::Other(format!("{operation} failed ({})", status.as_u16()))
}

async fn reserve_loopback_port() -> Result<u16, EngineError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn peer_status_serialization_never_exposes_tailcat_address() {
        let status = PeerStatus {
            signed_in: true,
            hosting: true,
            connected: true,
            profile_id: Some(uuid::Uuid::new_v4().to_string()),
            device_id: Some("device".into()),
            address: Some(format!("tc{}", "a".repeat(80))),
            derp_map: None,
            last_error: None,
        };
        let value = serde_json::to_value(status).unwrap();
        assert!(value.get("address").is_none());
        assert!(value.get("profileId").is_some());
    }
}
