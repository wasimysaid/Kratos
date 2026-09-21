//! Private, generation-based disaster recovery for a hosted peer.
//!
//! A generation contains consistent SQLite snapshots plus every secret needed
//! to retain the peer profile, trusted-device authority, and stable Tailcat
//! address. The manifest is written last and the directory is then atomically
//! renamed into place. Restore is deliberately offline and refuses to merge
//! with an existing installation.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use kratos_sync::peer::PeerStore;

const FORMAT_VERSION: u32 = 1;
const MAX_SECRET_BYTES: u64 = 1024 * 1024;
const REQUIRED: [(&str, &str); 5] = [
    ("peer.sqlite", "peer/peer.sqlite"),
    ("auth.sqlite", "peer/auth.sqlite"),
    ("peer-device.json", "peer-device.json"),
    ("peer-session.json", "peer-session.json"),
    ("tailcat-server.key", "peer/tailcat-server.key"),
];

pub const TRUST_ROLLBACK_WARNING: &str = "Restoring an older authorization database also restores its trusted-device and revocation state. Revoke or rotate any device compromised after this backup before exposing the restored peer; create a new profile if the host authority itself was compromised.";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("peer snapshot: {0}")]
    Peer(String),
    #[error("invalid peer backup: {0}")]
    Invalid(String),
    #[error("restore target is not empty")]
    TargetNotEmpty,
    #[error("restore requires explicit acknowledgement of authorization rollback risk")]
    TrustRollbackNotAcknowledged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupFile {
    pub path: String,
    pub length: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifest {
    pub format_version: u32,
    pub generation_id: String,
    pub created_at_ms: i64,
    pub profile_id: String,
    pub device_id: String,
    pub tailcat_address: String,
    pub trust_rollback_warning: String,
    pub files: Vec<BackupFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Latest {
    generation_id: String,
    manifest_sha256: String,
}

/// Create and atomically publish one complete hosted-peer generation.
pub fn create_generation(data_dir: &Path, peer_store: &PeerStore) -> Result<PathBuf, BackupError> {
    let backup_root = data_dir.join("peer/backups");
    private_dir(&backup_root)?;
    let generation_id = uuid::Uuid::new_v4().to_string();
    let stage = backup_root.join(format!(".{generation_id}.tmp"));
    let generation = backup_root.join(&generation_id);
    private_dir(&stage)?;

    let result = (|| {
        let stable_before = read_stable_files(data_dir)?;
        let session: serde_json::Value = serde_json::from_slice(
            stable_before
                .get("peer-session.json")
                .ok_or_else(|| BackupError::Invalid("missing peer session".into()))?,
        )?;
        if session.get("hosting").and_then(|v| v.as_bool()) != Some(true) {
            return Err(BackupError::Invalid(
                "only a hosted peer can produce a server backup".into(),
            ));
        }
        let profile_id = json_string(&session, "profileId")?;
        let device_id = json_string(&session, "deviceId")?;
        let tailcat_address = json_string(&session, "address")?;

        snapshot_sqlite(
            &data_dir.join("peer/auth.sqlite"),
            &stage.join("auth.sqlite"),
        )?;
        peer_store
            .backup_to(stage.join("peer.sqlite"))
            .map_err(|e| BackupError::Peer(e.to_string()))?;
        private_file_mode(&stage.join("peer.sqlite"))?;

        sync_regular_file(&stage.join("peer.sqlite"))?;

        if stable_before != read_stable_files(data_dir)? {
            return Err(BackupError::Invalid(
                "peer identity changed while backup was running; retry".into(),
            ));
        }
        for (name, bytes) in stable_before {
            private_write(&stage.join(name), &bytes)?;
        }

        let mut files = Vec::new();
        for (archive, _) in REQUIRED {
            files.push(file_record(&stage, archive)?);
        }
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            generation_id: generation_id.clone(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            profile_id,
            device_id,
            tailcat_address,
            trust_rollback_warning: TRUST_ROLLBACK_WARNING.into(),
            files,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        private_write(&stage.join("manifest.json"), &manifest_bytes)?;
        publish_directory(&stage, &generation)?;

        let latest = Latest {
            generation_id,
            manifest_sha256: hex_digest(&manifest_bytes),
        };
        atomic_private_write(
            &backup_root.join("latest.json"),
            &serde_json::to_vec_pretty(&latest)?,
        )?;
        Ok(generation.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Validate a generation without restoring it.
pub fn verify_generation(generation: &Path) -> Result<BackupManifest, BackupError> {
    if !fs::symlink_metadata(generation)?.file_type().is_dir() {
        return Err(BackupError::Invalid("generation is not a directory".into()));
    }
    let manifest_path = generation.join("manifest.json");
    let manifest: BackupManifest =
        serde_json::from_slice(&read_regular(&manifest_path, MAX_SECRET_BYTES)?)?;
    if manifest.format_version != FORMAT_VERSION
        || uuid::Uuid::parse_str(&manifest.generation_id).is_err()
        || generation.file_name().and_then(|v| v.to_str()) != Some(&manifest.generation_id)
        || manifest.trust_rollback_warning != TRUST_ROLLBACK_WARNING
    {
        return Err(BackupError::Invalid(
            "manifest identity or version mismatch".into(),
        ));
    }
    let expected: BTreeMap<_, _> = REQUIRED
        .iter()
        .map(|(a, _)| ((*a).to_string(), ()))
        .collect();
    let actual: BTreeMap<_, _> = manifest
        .files
        .iter()
        .map(|f| (f.path.clone(), ()))
        .collect();
    if actual != expected || manifest.files.len() != REQUIRED.len() {
        return Err(BackupError::Invalid("manifest inventory mismatch".into()));
    }
    for entry in &manifest.files {
        if entry.path.contains('/') || entry.path.contains('\\') || entry.path == "manifest.json" {
            return Err(BackupError::Invalid("unsafe manifest path".into()));
        }
        let bytes = read_regular(&generation.join(&entry.path), u64::MAX)?;
        if bytes.len() as u64 != entry.length || hex_digest(&bytes) != entry.sha256 {
            return Err(BackupError::Invalid(format!(
                "digest mismatch: {}",
                entry.path
            )));
        }
    }
    check_sqlite(&generation.join("peer.sqlite"))?;
    check_sqlite(&generation.join("auth.sqlite"))?;
    let session: serde_json::Value = serde_json::from_slice(&read_regular(
        &generation.join("peer-session.json"),
        MAX_SECRET_BYTES,
    )?)?;
    if json_string(&session, "profileId")? != manifest.profile_id
        || json_string(&session, "deviceId")? != manifest.device_id
        || json_string(&session, "address")? != manifest.tailcat_address
        || session.get("hosting").and_then(|v| v.as_bool()) != Some(true)
    {
        return Err(BackupError::Invalid("session binding mismatch".into()));
    }
    Ok(manifest)
}

/// Restore into an absent or empty data root. The caller must stop every Kratos
/// process using either source or destination before invoking this function.
pub fn restore_generation(
    generation: &Path,
    data_dir: &Path,
    acknowledge_trust_rollback: bool,
) -> Result<BackupManifest, BackupError> {
    if !acknowledge_trust_rollback {
        return Err(BackupError::TrustRollbackNotAcknowledged);
    }
    let manifest = verify_generation(generation)?;
    if data_dir.exists() && fs::read_dir(data_dir)?.next().is_some() {
        return Err(BackupError::TargetNotEmpty);
    }
    let parent = data_dir
        .parent()
        .ok_or_else(|| BackupError::Invalid("restore target has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let stage = parent.join(format!(".peer-restore-{}.tmp", uuid::Uuid::new_v4()));
    private_dir(&stage)?;
    private_dir(&stage.join("peer"))?;
    let result = (|| {
        for (archive, destination) in REQUIRED {
            let bytes = read_regular(&generation.join(archive), u64::MAX)?;
            let path = stage.join(destination);
            if let Some(parent) = path.parent() {
                private_dir(parent)?;
            }
            private_write(&path, &bytes)?;
        }
        // The nested peer directory will not be prepared by publishing its parent.
        prepare_directory_publication(&stage.join("peer"))?;
        if data_dir.exists() {
            fs::remove_dir(data_dir)?;
        }
        publish_directory(&stage, data_dir)?;
        Ok(manifest.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn read_stable_files(data_dir: &Path) -> Result<BTreeMap<&'static str, Vec<u8>>, BackupError> {
    let mut files = BTreeMap::new();
    for (archive, destination) in REQUIRED.iter().skip(2) {
        files.insert(
            *archive,
            read_regular(&data_dir.join(destination), MAX_SECRET_BYTES)?,
        );
    }
    Ok(files)
}

fn snapshot_sqlite(source: &Path, destination: &Path) -> Result<(), BackupError> {
    if destination.exists() {
        return Err(BackupError::Invalid("snapshot destination exists".into()));
    }
    let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute(
        "VACUUM INTO ?1",
        params![destination.to_string_lossy().as_ref()],
    )?;
    private_file_mode(destination)?;
    sync_regular_file(destination)?;
    Ok(())
}

fn check_sqlite(path: &Path) -> Result<(), BackupError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let result: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(BackupError::Invalid(format!(
            "SQLite integrity failed: {result}"
        )));
    }
    Ok(())
}

fn file_record(root: &Path, name: &str) -> Result<BackupFile, BackupError> {
    let bytes = read_regular(&root.join(name), u64::MAX)?;
    Ok(BackupFile {
        path: name.into(),
        length: bytes.len() as u64,
        sha256: hex_digest(&bytes),
    })
}

fn json_string(value: &serde_json::Value, key: &str) -> Result<String, BackupError> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| BackupError::Invalid(format!("missing session {key}")))
}

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, BackupError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(BackupError::Invalid(format!(
            "invalid backup file {}",
            path.display()
        )));
    }
    Ok(fs::read(path)?)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn private_dir(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file_mode(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_regular_file(path: &Path) -> Result<(), std::io::Error> {
    // FlushFileBuffers requires GENERIC_WRITE on Windows; File::open is
    // read-only there and fails with ERROR_ACCESS_DENIED.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    private_write(&tmp, bytes)?;
    let result = replace_file(&tmp, path);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn prepare_directory_publication(path: &Path) -> Result<(), std::io::Error> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "publication source is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        return File::open(path)?.sync_all();
    }
    #[cfg(windows)]
    {
        // Windows does not support POSIX-style directory fsync. Every regular
        // file is flushed before this point; publication itself uses
        // MoveFileExW(MOVEFILE_WRITE_THROUGH) below.
        Ok(())
    }
}

/// Atomically publishes a fully flushed directory within one parent.
pub fn publish_directory(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    if source.parent() != destination.parent() || destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "publication requires an absent same-directory destination",
        ));
    }
    prepare_directory_publication(source)?;
    move_path(source, destination, false)?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    move_path(source, destination, true)?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn move_path(source: &Path, destination: &Path, _replace: bool) -> Result<(), std::io::Error> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn move_path(source: &Path, destination: &Path, replace: bool) -> Result<(), std::io::Error> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
