//! Offline wrappers for hosted-peer disaster recovery.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kratos_engine::peer_runtime::backup::{
    BackupManifest, TRUST_ROLLBACK_WARNING, create_generation, restore_generation,
    verify_generation,
};
use kratos_engine::{EngineConfig, InstanceLock};
use kratos_sync::peer::PeerStore;

pub fn backup(config: &EngineConfig, output_dir: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        config.data_dir.is_dir(),
        "peer data directory does not exist: {}",
        config.data_dir.display()
    );
    let _lock = InstanceLock::acquire(&config.data_dir)?;
    let peer_db = config.data_dir.join("peer/peer.sqlite");
    anyhow::ensure!(
        peer_db.is_file(),
        "hosted peer database does not exist: {}",
        peer_db.display()
    );
    let peer = PeerStore::open(&peer_db)?;
    let generation = create_generation(&config.data_dir, &peer)?;
    let manifest = verify_generation(&generation)?;
    let exported = export_generation(&generation, &manifest, output_dir)?;
    println!("Peer backup written to {}", exported.display());
    Ok(exported)
}

pub fn restore(
    generation: &Path,
    destination: &Path,
    acknowledge_trust_rollback: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        acknowledge_trust_rollback,
        "--acknowledge-trust-rollback is required: {TRUST_ROLLBACK_WARNING}"
    );
    let manifest = verify_generation(generation)?;
    anyhow::ensure!(
        !destination.exists(),
        "restore destination must be absent (existing data is never merged): {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("restore destination has no parent"))?;
    fs::create_dir_all(parent)?;
    let stage = parent.join(format!(
        ".kratos-peer-restore-{}-{}",
        std::process::id(),
        nonce()
    ));
    anyhow::ensure!(!stage.exists(), "restore staging path already exists");

    let result = (|| -> anyhow::Result<()> {
        restore_generation(generation, &stage, true)?;
        // Keep the ordinary lock across publication where directory renames can
        // carry an open locked inode. Windows rejects renaming a directory that
        // contains this open handle, so release it after all restored writes and
        // validation are complete but before the atomic no-replace publication.
        let lock = InstanceLock::acquire(&stage)?;
        anyhow::ensure!(
            !destination.exists(),
            "restore destination appeared during restore; nothing was overwritten"
        );
        #[cfg(windows)]
        drop(lock);
        kratos_engine::peer_runtime::backup::publish_directory(&stage, destination)?;
        #[cfg(not(windows))]
        drop(lock);
        Ok(())
    })();
    if result.is_err() && stage.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    result?;
    eprintln!("{TRUST_ROLLBACK_WARNING}");
    println!(
        "Peer backup {} restored to {}",
        manifest.generation_id,
        destination.display()
    );
    Ok(())
}

fn export_generation(
    generation: &Path,
    manifest: &BackupManifest,
    output_dir: &Path,
) -> anyhow::Result<PathBuf> {
    private_dir(output_dir)?;
    let destination = output_dir.join(&manifest.generation_id);
    if destination == generation {
        return Ok(destination);
    }
    anyhow::ensure!(
        !destination.exists(),
        "backup destination already exists: {}",
        destination.display()
    );
    let stage = output_dir.join(format!(".{}.tmp-{}", manifest.generation_id, nonce()));
    private_dir(&stage)?;
    let result = (|| -> anyhow::Result<()> {
        for name in manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .chain(std::iter::once("manifest.json"))
        {
            let source = generation.join(name);
            let destination = stage.join(name);
            fs::copy(&source, &destination)?;
            private_file(&destination)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&destination)?
                .sync_all()?;
        }
        kratos_engine::peer_runtime::backup::publish_directory(&stage, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result?;
    verify_generation(&destination)?;
    Ok(destination)
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_engine::peer_auth::{AuthStore, DeviceIdentity};

    fn config(data_dir: &Path) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.to_path_buf(),
            ipc_port: 0,
            default_harness: kratos_engine::HarnessId::Mock,
        }
    }

    fn seed_host(root: &Path) {
        fs::create_dir_all(root.join("peer")).unwrap();
        let identity = DeviceIdentity::load_or_create(root.join("peer-device.json")).unwrap();
        let auth = AuthStore::open(root.join("peer/auth.sqlite")).unwrap();
        let principal = auth
            .create_profile(&identity.public_key(), Some("backup owner"))
            .unwrap();
        fs::write(
            root.join("peer-session.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "profileId": principal.profile_id,
                "deviceId": principal.device_id,
                "address": format!("tc{}", "a".repeat(80)),
                "hosting": true,
                "localPort": 27191,
                "derpMap": null
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("peer/tailcat-server.key"),
            br#"{"role":"server","private_key":"secret","preshared_key":"pair","region":{"RegionID":1}}"#,
        )
        .unwrap();
        PeerStore::open(root.join("peer/peer.sqlite")).unwrap();
    }

    #[test]
    fn backup_and_restore_use_explicit_paths_and_preserve_generation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        seed_host(&source);
        let exported = backup(&config(&source), &temp.path().join("offline-backups")).unwrap();
        let original = verify_generation(&exported).unwrap();

        let restored = temp.path().join("restored-data");
        restore(&exported, &restored, true).unwrap();
        assert_eq!(
            verify_generation(&exported).unwrap().generation_id,
            original.generation_id
        );
        assert!(restored.join("peer/peer.sqlite").is_file());
        assert!(restored.join("peer/auth.sqlite").is_file());
        assert!(restored.join("engine.lock").is_file());
    }

    #[test]
    fn rejects_busy_backup_and_unsafe_restore_targets() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        seed_host(&source);
        let held = InstanceLock::acquire(&source).unwrap();
        let busy = backup(&config(&source), &temp.path().join("busy-output")).unwrap_err();
        assert!(busy.to_string().contains("already running"));
        drop(held);

        let exported = backup(&config(&source), &temp.path().join("backups")).unwrap();
        let no_ack_target = temp.path().join("no-ack");
        let no_ack = restore(&exported, &no_ack_target, false).unwrap_err();
        assert!(no_ack.to_string().contains("--acknowledge-trust-rollback"));
        assert!(!no_ack_target.exists());

        let occupied = temp.path().join("occupied");
        fs::create_dir_all(&occupied).unwrap();
        fs::write(occupied.join("keep"), b"untouched").unwrap();
        let nonempty = restore(&exported, &occupied, true).unwrap_err();
        assert!(nonempty.to_string().contains("must be absent"));
        assert_eq!(fs::read(occupied.join("keep")).unwrap(), b"untouched");
    }
}
