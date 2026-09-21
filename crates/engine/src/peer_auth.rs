//! Application-level trusted-device authentication for self-hosted peers.
//!
//! Pairing and login use Ed25519 proofs. Random invitation, challenge, and
//! bearer values are never persisted in plaintext; SQLite stores SHA-256
//! digests only. Profile creation is deliberately not exposed by [`router`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use uuid::Uuid;

const REDEEM_DOMAIN: &[u8] = b"kratos.peer-auth.invite-redeem.v1\0";
const AUTH_DOMAIN: &[u8] = b"kratos.peer-auth.challenge.v1\0";
const IDENTITY_VERSION: u8 = 1;
const MAX_JSON_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credential")]
    InvalidCredential,
    #[error("credential expired")]
    Expired,
    #[error("credential was already used")]
    AlreadyUsed,
    #[error("authentication required")]
    Unauthorized,
    #[error("owner authorization required")]
    OwnerRequired,
    #[error("invalid profile id")]
    InvalidProfile,
    #[error("invalid device identity")]
    InvalidDevice,
    #[error("invalid display name")]
    InvalidName,
    #[error("device is revoked")]
    Revoked,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("authentication database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("identity file is malformed")]
    MalformedIdentity,
}

#[derive(Clone, Copy, Debug)]
pub struct AuthTtls {
    pub invite: Duration,
    pub challenge: Duration,
    pub token: Duration,
}

impl Default for AuthTtls {
    fn default() -> Self {
        Self {
            invite: Duration::from_secs(10 * 60),
            challenge: Duration::from_secs(2 * 60),
            token: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone)]
pub struct AuthStore {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    connection: Mutex<Connection>,
    ttls: AuthTtls,
    revoked: broadcast::Sender<Revocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub profile_id: String,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revocation {
    pub profile_id: String,
    pub device_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    pub version: u8,
    pub profile_id: String,
    pub invite_id: String,
    pub secret: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub profile_id: String,
    pub device_id: String,
    pub challenge_id: String,
    pub nonce: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemRequest {
    pub version: u8,
    pub profile_id: String,
    pub invite_id: String,
    pub secret: String,
    pub public_key: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeRequest {
    pub profile_id: String,
    pub device_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateRequest {
    pub profile_id: String,
    pub device_id: String,
    pub challenge_id: String,
    pub nonce: String,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    pub token: String,
    pub expires_at: i64,
    pub principal: Principal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub device_id: String,
    pub display_name: Option<String>,
    pub owner: bool,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InviteRequest {
    #[serde(default)]
    expires_in_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevokeRequest {
    device_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RedeemResponse {
    profile_id: String,
    device_id: String,
}

impl AuthStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        Self::open_with_ttls(path, AuthTtls::default())
    }

    pub fn open_with_ttls(path: impl AsRef<Path>, ttls: AuthTtls) -> Result<Self, AuthError> {
        if ttls.invite.is_zero() || ttls.challenge.is_zero() || ttls.token.is_zero() {
            return Err(AuthError::InvalidCredential);
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_profiles (
                profile_id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS auth_devices (
                profile_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                public_key BLOB NOT NULL,
                display_name TEXT,
                is_owner INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                revoked_at INTEGER,
                PRIMARY KEY (profile_id, device_id),
                FOREIGN KEY (profile_id) REFERENCES auth_profiles(profile_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS auth_invites (
                profile_id TEXT NOT NULL,
                invite_id TEXT NOT NULL,
                secret_hash BLOB NOT NULL,
                issuer_device_id TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                used_at INTEGER,
                PRIMARY KEY (profile_id, invite_id),
                FOREIGN KEY (profile_id, issuer_device_id)
                    REFERENCES auth_devices(profile_id, device_id)
             );
             CREATE INDEX IF NOT EXISTS auth_invites_secret
                ON auth_invites(profile_id, invite_id, secret_hash);
             CREATE TABLE IF NOT EXISTS auth_challenges (
                profile_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                challenge_id TEXT NOT NULL,
                nonce_hash BLOB NOT NULL,
                expires_at INTEGER NOT NULL,
                used_at INTEGER,
                PRIMARY KEY (profile_id, challenge_id),
                FOREIGN KEY (profile_id, device_id)
                    REFERENCES auth_devices(profile_id, device_id)
             );
             CREATE INDEX IF NOT EXISTS auth_challenges_nonce
                ON auth_challenges(profile_id, challenge_id, nonce_hash);
             CREATE TABLE IF NOT EXISTS auth_tokens (
                token_hash BLOB PRIMARY KEY,
                profile_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (profile_id, device_id)
                    REFERENCES auth_devices(profile_id, device_id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS auth_tokens_principal
                ON auth_tokens(profile_id, device_id);",
        )?;
        let (revoked, _) = broadcast::channel(64);
        Ok(Self {
            inner: Arc::new(AuthInner {
                connection: Mutex::new(connection),
                ttls,
                revoked,
            }),
        })
    }

    /// Local-only owner bootstrap. No HTTP route calls this method.
    pub fn create_profile(
        &self,
        owner_public_key: &[u8; 32],
        display_name: Option<&str>,
    ) -> Result<Principal, AuthError> {
        validate_name(display_name)?;
        VerifyingKey::from_bytes(owner_public_key).map_err(|_| AuthError::InvalidDevice)?;
        let profile_id = Uuid::new_v4().to_string();
        let device_id = device_id(owner_public_key);
        let now = unix_now();
        let mut connection = self.connection();
        let tx = connection.transaction()?;
        tx.execute(
            "INSERT INTO auth_profiles(profile_id, created_at) VALUES (?1, ?2)",
            params![profile_id, now],
        )?;
        tx.execute(
            "INSERT INTO auth_devices
             (profile_id, device_id, public_key, display_name, is_owner, created_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)",
            params![
                profile_id,
                device_id,
                owner_public_key.as_slice(),
                display_name,
                now
            ],
        )?;
        tx.commit()?;
        Ok(Principal {
            profile_id,
            device_id,
        })
    }

    /// Create a one-use invitation. The issuer must be an active profile owner.
    pub fn create_invite(
        &self,
        profile_id: &str,
        issuer_device_id: &str,
    ) -> Result<Invite, AuthError> {
        self.create_invite_for(profile_id, issuer_device_id, self.inner.ttls.invite)
    }

    fn create_invite_for(
        &self,
        profile_id: &str,
        issuer_device_id: &str,
        ttl: Duration,
    ) -> Result<Invite, AuthError> {
        validate_profile(profile_id)?;
        validate_device_id(issuer_device_id)?;
        let ttl = ttl.min(self.inner.ttls.invite);
        if ttl.is_zero() {
            return Err(AuthError::InvalidCredential);
        }
        let connection = self.connection();
        require_owner(&connection, profile_id, issuer_device_id)?;
        let invite_id = Uuid::new_v4().to_string();
        let secret_bytes = random_bytes();
        let secret = encode(&secret_bytes);
        let expires_at = expiry(ttl);
        connection.execute(
            "INSERT INTO auth_invites
             (profile_id, invite_id, secret_hash, issuer_device_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                profile_id,
                invite_id,
                digest(&secret_bytes),
                issuer_device_id,
                expires_at
            ],
        )?;
        Ok(Invite {
            version: IDENTITY_VERSION,
            profile_id: profile_id.to_owned(),
            invite_id,
            secret,
            expires_at,
        })
    }

    pub fn redeem_invite(&self, request: &RedeemRequest) -> Result<Principal, AuthError> {
        validate_profile(&request.profile_id)?;
        validate_uuid(&request.invite_id).map_err(|_| AuthError::InvalidCredential)?;
        validate_name(request.display_name.as_deref())?;
        if request.version != IDENTITY_VERSION {
            return Err(AuthError::InvalidCredential);
        }
        let secret = decode_array::<32>(&request.secret)?;
        let public_key = decode_array::<32>(&request.public_key)?;
        let signature = decode_signature(&request.signature)?;
        let verifying_key =
            VerifyingKey::from_bytes(&public_key).map_err(|_| AuthError::InvalidCredential)?;
        verifying_key
            .verify(
                &redeem_payload(
                    request.version,
                    &request.profile_id,
                    &request.invite_id,
                    &secret,
                    &public_key,
                ),
                &signature,
            )
            .map_err(|_| AuthError::InvalidCredential)?;

        let now = unix_now();
        let mut connection = self.connection();
        let tx = connection.transaction()?;
        let invite = tx
            .query_row(
                "SELECT expires_at, used_at FROM auth_invites
                 WHERE profile_id = ?1 AND invite_id = ?2 AND secret_hash = ?3",
                params![request.profile_id, request.invite_id, digest(&secret)],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        let Some((expires_at, used_at)) = invite else {
            return Err(AuthError::InvalidCredential);
        };
        if used_at.is_some() {
            return Err(AuthError::AlreadyUsed);
        }
        if expires_at <= now {
            return Err(AuthError::Expired);
        }
        let device_id = device_id(&public_key);
        let existing = tx
            .query_row(
                "SELECT public_key, revoked_at FROM auth_devices
                 WHERE profile_id = ?1 AND device_id = ?2",
                params![request.profile_id, device_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        if let Some((existing_key, revoked_at)) = &existing {
            if existing_key.as_slice() != public_key || revoked_at.is_some() {
                // Revoked identities are never silently revived. Recovery is
                // explicit: remove the old identity and pair with a fresh key.
                return Err(AuthError::Revoked);
            }
        }

        let changed = tx.execute(
            "UPDATE auth_invites SET used_at = ?1
             WHERE profile_id = ?2 AND invite_id = ?3 AND used_at IS NULL",
            params![now, request.profile_id, request.invite_id],
        )?;
        if changed != 1 {
            return Err(AuthError::AlreadyUsed);
        }
        if existing.is_some() {
            tx.execute(
                "UPDATE auth_devices SET display_name = COALESCE(?1, display_name)
                 WHERE profile_id = ?2 AND device_id = ?3 AND revoked_at IS NULL",
                params![request.display_name, request.profile_id, device_id],
            )?;
        } else {
            tx.execute(
                "INSERT INTO auth_devices
                 (profile_id, device_id, public_key, display_name, is_owner, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![
                    request.profile_id,
                    device_id,
                    public_key.as_slice(),
                    request.display_name,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(Principal {
            profile_id: request.profile_id.clone(),
            device_id,
        })
    }

    pub fn create_challenge(
        &self,
        profile_id: &str,
        device_id: &str,
    ) -> Result<Challenge, AuthError> {
        validate_profile(profile_id)?;
        validate_device_id(device_id)?;
        let connection = self.connection();
        active_public_key(&connection, profile_id, device_id)?;
        let challenge_id = Uuid::new_v4().to_string();
        let nonce_bytes = random_bytes();
        let nonce = encode(&nonce_bytes);
        let expires_at = expiry(self.inner.ttls.challenge);
        connection.execute(
            "INSERT INTO auth_challenges
             (profile_id, device_id, challenge_id, nonce_hash, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                profile_id,
                device_id,
                challenge_id,
                digest(&nonce_bytes),
                expires_at
            ],
        )?;
        Ok(Challenge {
            profile_id: profile_id.to_owned(),
            device_id: device_id.to_owned(),
            challenge_id,
            nonce,
            expires_at,
        })
    }

    pub fn authenticate_challenge(
        &self,
        request: &AuthenticateRequest,
    ) -> Result<TokenResponse, AuthError> {
        validate_profile(&request.profile_id)?;
        validate_device_id(&request.device_id)?;
        validate_uuid(&request.challenge_id).map_err(|_| AuthError::InvalidCredential)?;
        let nonce = decode_array::<32>(&request.nonce)?;
        let signature = decode_signature(&request.signature)?;
        let now = unix_now();
        let mut connection = self.connection();
        let tx = connection.transaction()?;
        let row = challenge_row(
            &tx,
            &request.profile_id,
            &request.device_id,
            &request.challenge_id,
            &digest(&nonce),
        )?;
        let Some((public_key, expires_at, used_at, revoked_at)) = row else {
            return Err(AuthError::InvalidCredential);
        };
        if revoked_at.is_some() {
            return Err(AuthError::Revoked);
        }
        if used_at.is_some() {
            return Err(AuthError::AlreadyUsed);
        }
        if expires_at <= now {
            return Err(AuthError::Expired);
        }
        let public_key: [u8; 32] = public_key
            .try_into()
            .map_err(|_| AuthError::InvalidCredential)?;
        if device_id(&public_key) != request.device_id {
            return Err(AuthError::InvalidDevice);
        }
        VerifyingKey::from_bytes(&public_key)
            .map_err(|_| AuthError::InvalidCredential)?
            .verify(
                &auth_payload(
                    &request.profile_id,
                    &request.device_id,
                    &request.challenge_id,
                    &nonce,
                ),
                &signature,
            )
            .map_err(|_| AuthError::InvalidCredential)?;
        let changed = tx.execute(
            "UPDATE auth_challenges SET used_at = ?1
             WHERE profile_id = ?2 AND challenge_id = ?3 AND used_at IS NULL",
            params![now, request.profile_id, request.challenge_id],
        )?;
        if changed != 1 {
            return Err(AuthError::AlreadyUsed);
        }
        let token_bytes = random_bytes();
        let token = encode(&token_bytes);
        let token_expires = expiry(self.inner.ttls.token);
        tx.execute(
            "INSERT INTO auth_tokens(token_hash, profile_id, device_id, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                digest(&token_bytes),
                request.profile_id,
                request.device_id,
                token_expires,
                now
            ],
        )?;
        tx.commit()?;
        Ok(TokenResponse {
            token,
            expires_at: token_expires,
            principal: Principal {
                profile_id: request.profile_id.clone(),
                device_id: request.device_id.clone(),
            },
        })
    }

    /// Authenticate a short-lived bearer via a digest-indexed lookup.
    pub fn authenticate(&self, token: &str) -> Result<Principal, AuthError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(AuthError::Unauthorized);
        }
        let token = decode_array::<32>(token).map_err(|_| AuthError::Unauthorized)?;
        let now = unix_now();
        let connection = self.connection();
        let principal = connection
            .query_row(
                "SELECT t.profile_id, t.device_id
                 FROM auth_tokens t JOIN auth_devices d
                   ON d.profile_id = t.profile_id AND d.device_id = t.device_id
                 WHERE t.token_hash = ?1 AND t.expires_at > ?2 AND d.revoked_at IS NULL",
                params![digest(&token), now],
                |row| {
                    Ok(Principal {
                        profile_id: row.get(0)?,
                        device_id: row.get(1)?,
                    })
                },
            )
            .optional()?;
        principal.ok_or(AuthError::Unauthorized)
    }

    pub fn list_devices(&self, principal: &Principal) -> Result<Vec<DeviceRecord>, AuthError> {
        let connection = self.connection();
        require_owner(&connection, &principal.profile_id, &principal.device_id)?;
        let mut statement = connection.prepare(
            "SELECT device_id, display_name, is_owner, created_at, revoked_at
             FROM auth_devices WHERE profile_id = ?1 ORDER BY created_at, device_id",
        )?;
        let rows = statement.query_map([&principal.profile_id], |row| {
            Ok(DeviceRecord {
                device_id: row.get(0)?,
                display_name: row.get(1)?,
                owner: row.get::<_, i64>(2)? != 0,
                created_at: row.get(3)?,
                revoked_at: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AuthError::from)
    }

    pub fn revoke_device(
        &self,
        principal: &Principal,
        target_device_id: &str,
    ) -> Result<(), AuthError> {
        validate_device_id(target_device_id)?;
        let now = unix_now();
        let mut connection = self.connection();
        let tx = connection.transaction()?;
        require_owner_tx(&tx, &principal.profile_id, &principal.device_id)?;
        let changed = tx.execute(
            "UPDATE auth_devices SET revoked_at = ?1
             WHERE profile_id = ?2 AND device_id = ?3 AND revoked_at IS NULL",
            params![now, principal.profile_id, target_device_id],
        )?;
        if changed != 1 {
            return Err(AuthError::InvalidDevice);
        }
        tx.execute(
            "DELETE FROM auth_tokens WHERE profile_id = ?1 AND device_id = ?2",
            params![principal.profile_id, target_device_id],
        )?;
        tx.commit()?;
        let _ = self.inner.revoked.send(Revocation {
            profile_id: principal.profile_id.clone(),
            device_id: target_device_id.to_owned(),
        });
        Ok(())
    }

    pub fn revoked_devices(&self) -> Result<Vec<Revocation>, AuthError> {
        let connection = self.connection();
        let mut statement = connection.prepare(
            "SELECT profile_id, device_id FROM auth_devices WHERE revoked_at IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Revocation {
                profile_id: row.get(0)?,
                device_id: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AuthError::from)
    }

    pub fn subscribe_revocations(&self) -> broadcast::Receiver<Revocation> {
        self.inner.revoked.subscribe()
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.inner
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Persistent client signing identity. The file contains a versioned 32-byte
/// secret seed and is atomically replaced with owner-only permissions.
pub struct DeviceIdentity {
    signing_key: SigningKey,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    version: u8,
    secret_key: String,
}

impl DeviceIdentity {
    /// Generate a replacement credential without touching the durable identity.
    /// Pairing uses this to prove a fresh key before publishing it locally.
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), AuthError> {
        let stored = StoredIdentity {
            version: IDENTITY_VERSION,
            secret_key: encode(&self.signing_key.to_bytes()),
        };
        let bytes = serde_json::to_vec(&stored).map_err(|_| AuthError::MalformedIdentity)?;
        atomic_private_write(path.as_ref(), &bytes)
    }
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => {
                set_private_permissions(path)?;
                let stored: StoredIdentity =
                    serde_json::from_slice(&bytes).map_err(|_| AuthError::MalformedIdentity)?;
                if stored.version != IDENTITY_VERSION {
                    return Err(AuthError::MalformedIdentity);
                }
                let secret = decode_array::<32>(&stored.secret_key)
                    .map_err(|_| AuthError::MalformedIdentity)?;
                Ok(Self {
                    signing_key: SigningKey::from_bytes(&secret),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
                if let Some(parent) = parent {
                    std::fs::create_dir_all(parent)?;
                }
                let signing_key = SigningKey::generate(&mut OsRng);
                let stored = StoredIdentity {
                    version: IDENTITY_VERSION,
                    secret_key: encode(&signing_key.to_bytes()),
                };
                let bytes =
                    serde_json::to_vec(&stored).map_err(|_| AuthError::MalformedIdentity)?;
                atomic_private_write(path, &bytes)?;
                Ok(Self { signing_key })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn device_id(&self) -> String {
        device_id(&self.public_key())
    }

    pub fn redeem_request(
        &self,
        invite: &Invite,
        display_name: Option<String>,
    ) -> Result<RedeemRequest, AuthError> {
        validate_profile(&invite.profile_id)?;
        validate_uuid(&invite.invite_id).map_err(|_| AuthError::InvalidCredential)?;
        validate_name(display_name.as_deref())?;
        let secret = decode_array::<32>(&invite.secret)?;
        let public_key = self.public_key();
        let signature = self.signing_key.sign(&redeem_payload(
            invite.version,
            &invite.profile_id,
            &invite.invite_id,
            &secret,
            &public_key,
        ));
        Ok(RedeemRequest {
            version: invite.version,
            profile_id: invite.profile_id.clone(),
            invite_id: invite.invite_id.clone(),
            secret: invite.secret.clone(),
            public_key: encode(&public_key),
            signature: encode(&signature.to_bytes()),
            display_name,
        })
    }

    pub fn sign_challenge(&self, challenge: &Challenge) -> Result<AuthenticateRequest, AuthError> {
        validate_profile(&challenge.profile_id)?;
        validate_uuid(&challenge.challenge_id).map_err(|_| AuthError::InvalidCredential)?;
        let nonce = decode_array::<32>(&challenge.nonce)?;
        let signature = self.signing_key.sign(&auth_payload(
            &challenge.profile_id,
            &challenge.device_id,
            &challenge.challenge_id,
            &nonce,
        ));
        Ok(AuthenticateRequest {
            profile_id: challenge.profile_id.clone(),
            device_id: challenge.device_id.clone(),
            challenge_id: challenge.challenge_id.clone(),
            nonce: challenge.nonce.clone(),
            signature: encode(&signature.to_bytes()),
        })
    }
}

/// Pairing HTTP API. Profile bootstrap remains a local Rust API only.
pub fn router(store: AuthStore) -> Router {
    Router::new()
        .route("/pair/redeem", post(http_redeem))
        .route("/pair/challenge", post(http_challenge))
        .route("/pair/authenticate", post(http_authenticate))
        .route("/pair/invite", post(http_invite))
        .route("/pair/devices", get(http_devices))
        .route("/pair/revoke", post(http_revoke))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
        .with_state(store)
}

/// Middleware for protected peer routes. A valid bearer is converted to the
/// principal type consumed by `kratos_sync::peer` handlers.
pub async fn require_peer_principal(
    State(store): State<AuthStore>,
    mut request: Request,
    next: Next,
) -> Response {
    let result = request_bearer(&request).and_then(|token| store.authenticate(token));
    match result {
        Ok(principal) => {
            request
                .extensions_mut()
                .insert(kratos_sync::peer::PeerPrincipal {
                    profile_id: principal.profile_id,
                    device_id: principal.device_id,
                });
            next.run(request).await
        }
        Err(error) => api_error(error),
    }
}

async fn http_redeem(
    State(store): State<AuthStore>,
    Json(request): Json<RedeemRequest>,
) -> Response {
    match store.redeem_invite(&request) {
        Ok(principal) => Json(RedeemResponse {
            profile_id: principal.profile_id,
            device_id: principal.device_id,
        })
        .into_response(),
        Err(error) => api_error(error),
    }
}

async fn http_challenge(
    State(store): State<AuthStore>,
    Json(request): Json<ChallengeRequest>,
) -> Response {
    match store.create_challenge(&request.profile_id, &request.device_id) {
        Ok(challenge) => Json(challenge).into_response(),
        Err(error) => api_error(error),
    }
}

async fn http_authenticate(
    State(store): State<AuthStore>,
    Json(request): Json<AuthenticateRequest>,
) -> Response {
    match store.authenticate_challenge(&request) {
        Ok(token) => Json(token).into_response(),
        Err(error) => api_error(error),
    }
}

async fn http_invite(
    State(store): State<AuthStore>,
    headers: HeaderMap,
    Json(request): Json<InviteRequest>,
) -> Response {
    let result = authenticated(&store, &headers).and_then(|principal| {
        let ttl = request
            .expires_in_seconds
            .map(Duration::from_secs)
            .unwrap_or(store.inner.ttls.invite);
        store.create_invite_for(&principal.profile_id, &principal.device_id, ttl)
    });
    match result {
        Ok(invite) => Json(invite).into_response(),
        Err(error) => api_error(error),
    }
}

async fn http_devices(State(store): State<AuthStore>, headers: HeaderMap) -> Response {
    let result =
        authenticated(&store, &headers).and_then(|principal| store.list_devices(&principal));
    match result {
        Ok(devices) => Json(devices).into_response(),
        Err(error) => api_error(error),
    }
}

async fn http_revoke(
    State(store): State<AuthStore>,
    headers: HeaderMap,
    Json(request): Json<RevokeRequest>,
) -> Response {
    let result = authenticated(&store, &headers)
        .and_then(|principal| store.revoke_device(&principal, &request.device_id));
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => api_error(error),
    }
}

fn authenticated(store: &AuthStore, headers: &HeaderMap) -> Result<Principal, AuthError> {
    store.authenticate(bearer(headers)?)
}
fn request_bearer(request: &Request) -> Result<&str, AuthError> {
    if let Ok(token) = bearer(request.headers()) {
        return Ok(token);
    }
    // Existing registry/chat transports derive both WebSocket and HTTP
    // fallback URLs from the same tokenized room URL. Accept that URL-safe
    // bearer on protected peer routes when no Authorization header is present.
    let mut found = None;
    for pair in request.uri().query().unwrap_or_default().split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == "token" {
            if found.replace(value).is_some() {
                return Err(AuthError::Unauthorized);
            }
        }
    }
    validate_bearer(found.ok_or(AuthError::Unauthorized)?)
}

fn bearer(headers: &HeaderMap) -> Result<&str, AuthError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(AuthError::Unauthorized)?;
    let (scheme, token) = value.split_once(' ').ok_or(AuthError::Unauthorized)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Unauthorized);
    }
    validate_bearer(token)
}

fn validate_bearer(token: &str) -> Result<&str, AuthError> {
    if token.is_empty()
        || token.contains(char::is_whitespace)
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AuthError::Unauthorized);
    }
    Ok(token)
}

fn api_error(error: AuthError) -> Response {
    let status = match error {
        AuthError::Unauthorized | AuthError::InvalidCredential => StatusCode::UNAUTHORIZED,
        AuthError::OwnerRequired => StatusCode::FORBIDDEN,
        AuthError::Revoked => StatusCode::FORBIDDEN,
        AuthError::Expired | AuthError::AlreadyUsed => StatusCode::CONFLICT,
        AuthError::InvalidProfile
        | AuthError::InvalidDevice
        | AuthError::InvalidName
        | AuthError::MalformedIdentity => StatusCode::BAD_REQUEST,
        AuthError::Io(_) | AuthError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let code = match status {
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::CONFLICT => "credential_unavailable",
        StatusCode::BAD_REQUEST => "invalid_request",
        _ => "internal_error",
    };
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

fn challenge_row(
    tx: &Transaction<'_>,
    profile_id: &str,
    device_id: &str,
    challenge_id: &str,
    nonce_hash: &[u8; 32],
) -> Result<Option<(Vec<u8>, i64, Option<i64>, Option<i64>)>, AuthError> {
    tx.query_row(
        "SELECT d.public_key, c.expires_at, c.used_at, d.revoked_at
         FROM auth_challenges c JOIN auth_devices d
           ON d.profile_id = c.profile_id AND d.device_id = c.device_id
         WHERE c.profile_id = ?1 AND c.device_id = ?2
           AND c.challenge_id = ?3 AND c.nonce_hash = ?4",
        params![profile_id, device_id, challenge_id, nonce_hash.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .map_err(AuthError::from)
}

fn require_owner(connection: &Connection, profile: &str, device: &str) -> Result<(), AuthError> {
    let owner = connection
        .query_row(
            "SELECT is_owner FROM auth_devices
             WHERE profile_id = ?1 AND device_id = ?2 AND revoked_at IS NULL",
            params![profile, device],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match owner {
        Some(1) => Ok(()),
        Some(_) => Err(AuthError::OwnerRequired),
        None => Err(AuthError::Unauthorized),
    }
}

fn require_owner_tx(tx: &Transaction<'_>, profile: &str, device: &str) -> Result<(), AuthError> {
    let owner = tx
        .query_row(
            "SELECT is_owner FROM auth_devices
             WHERE profile_id = ?1 AND device_id = ?2 AND revoked_at IS NULL",
            params![profile, device],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match owner {
        Some(1) => Ok(()),
        Some(_) => Err(AuthError::OwnerRequired),
        None => Err(AuthError::Unauthorized),
    }
}

fn active_public_key(
    connection: &Connection,
    profile: &str,
    device: &str,
) -> Result<Vec<u8>, AuthError> {
    connection
        .query_row(
            "SELECT public_key FROM auth_devices
             WHERE profile_id = ?1 AND device_id = ?2 AND revoked_at IS NULL",
            params![profile, device],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(AuthError::InvalidCredential)
}

fn redeem_payload(
    version: u8,
    profile: &str,
    invite_id: &str,
    secret: &[u8; 32],
    public_key: &[u8; 32],
) -> Vec<u8> {
    framed(
        REDEEM_DOMAIN,
        &[
            &[version],
            profile.as_bytes(),
            invite_id.as_bytes(),
            secret,
            public_key,
        ],
    )
}

fn auth_payload(profile: &str, device: &str, challenge_id: &str, nonce: &[u8; 32]) -> Vec<u8> {
    framed(
        AUTH_DOMAIN,
        &[
            profile.as_bytes(),
            device.as_bytes(),
            challenge_id.as_bytes(),
            nonce,
        ],
    )
}

fn framed(domain: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut output = Vec::with_capacity(
        domain.len() + fields.iter().map(|field| 4 + field.len()).sum::<usize>(),
    );
    output.extend_from_slice(domain);
    for field in fields {
        output.extend_from_slice(&(field.len() as u32).to_be_bytes());
        output.extend_from_slice(field);
    }
    output
}

fn random_bytes() -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn digest(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn device_id(public_key: &[u8; 32]) -> String {
    format!("dev_{}", encode(&digest(public_key)))
}

fn validate_device_id(value: &str) -> Result<(), AuthError> {
    let encoded = value.strip_prefix("dev_").ok_or(AuthError::InvalidDevice)?;
    decode_array::<32>(encoded)
        .map(|_| ())
        .map_err(|_| AuthError::InvalidDevice)
}

fn validate_profile(value: &str) -> Result<(), AuthError> {
    validate_uuid(value).map_err(|_| AuthError::InvalidProfile)
}

fn validate_uuid(value: &str) -> Result<(), ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.to_string() == value {
        Ok(())
    } else {
        Err(())
    }
}

fn validate_name(value: Option<&str>) -> Result<(), AuthError> {
    if let Some(value) = value
        && (value.is_empty() || value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control))
    {
        return Err(AuthError::InvalidName);
    }
    Ok(())
}

fn decode_array<const N: usize>(value: &str) -> Result<[u8; N], AuthError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| AuthError::InvalidCredential)?;
    bytes.try_into().map_err(|_| AuthError::InvalidCredential)
}

fn decode_signature(value: &str) -> Result<Signature, AuthError> {
    Ok(Signature::from_bytes(&decode_array::<64>(value)?))
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn expiry(ttl: Duration) -> i64 {
    unix_now().saturating_add(ttl.as_secs().min(i64::MAX as u64) as i64)
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut suffix = [0_u8; 8];
    OsRng.fill_bytes(&mut suffix);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(AuthError::MalformedIdentity)?;
    let temporary: PathBuf = parent.join(format!(".{file_name}.{}.tmp", encode(&suffix)));
    let result = (|| {
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
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}
