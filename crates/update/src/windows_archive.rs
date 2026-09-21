use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

pub(crate) const APP: &str = "kratos.exe";
pub(crate) const ADAPTER: &str = "kratos-tailcat.exe";
pub(crate) const CONFIG: &str = "kratos-update.json";

pub(crate) const PENDING: &str = ".kratos-update-pending";
pub(crate) const INCOMING_APP: &str = ".kratos-update-incoming-kratos.exe";
pub(crate) const INCOMING_ADAPTER: &str = ".kratos-update-incoming-kratos-tailcat.exe";
pub(crate) const INCOMING_CONFIG: &str = ".kratos-update-incoming-config.json";
pub(crate) const BACKUP_APP: &str = ".kratos-update-backup-kratos.exe";
pub(crate) const BACKUP_ADAPTER: &str = ".kratos-update-backup-kratos-tailcat.exe";
pub(crate) const BACKUP_CONFIG: &str = ".kratos-update-backup-config.json";
const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_SIZE: u64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Payload {
    pub app: PathBuf,
    pub adapter: PathBuf,
    pub config: PathBuf,
}

#[derive(serde::Deserialize)]
struct PackageConfig {
    releases_url: String,
    companion: Companion,
}

#[derive(serde::Deserialize)]
struct Companion {
    file: String,
    sha256: String,
}

pub(crate) fn extract(zip_path: &Path, root: &Path) -> anyhow::Result<Payload> {
    let file = File::open(zip_path).context("opening Windows update ZIP")?;
    let mut zip = zip::ZipArchive::new(file).context("parsing Windows update ZIP")?;
    ensure!(zip.len() <= 256, "Windows update ZIP has too many entries");
    fs::create_dir_all(root)?;
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .context("reading Windows update ZIP entry")?;
        let name = entry.name().to_owned();
        ensure!(safe_name(&name), "unsafe Windows update ZIP entry");
        ensure!(
            seen.insert(name.clone()),
            "duplicate Windows update ZIP entry"
        );
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0 || kind == 0o100000 || (entry.is_dir() && kind == 0o040000),
                "non-regular Windows update ZIP entry"
            );
        }
        ensure!(
            allowed(&name, entry.is_dir()),
            "unexpected Windows update ZIP entry"
        );
        if entry.is_dir() {
            fs::create_dir_all(root.join(&name))?;
            continue;
        }
        ensure!(
            entry.size() <= MAX_FILE_SIZE,
            "Windows update ZIP entry is too large"
        );
        total = total
            .checked_add(entry.size())
            .context("Windows update ZIP size overflow")?;
        ensure!(total <= MAX_TOTAL_SIZE, "Windows update ZIP is too large");
        let output = root.join(&name);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .context("creating Windows update payload file")?;
        let copied = std::io::copy(&mut entry.take(MAX_FILE_SIZE + 1), &mut out)?;
        ensure!(
            copied <= MAX_FILE_SIZE,
            "Windows update ZIP entry expanded beyond its declared size"
        );
        out.flush()?;
    }
    validate(root)
}

pub(crate) fn validate(root: &Path) -> anyhow::Result<Payload> {
    let payload = Payload {
        app: root.join(APP),
        adapter: root.join(ADAPTER),
        config: root.join(CONFIG),
    };
    ensure!(
        payload.app.is_file(),
        "Windows update ZIP is missing kratos.exe"
    );
    ensure!(
        payload.adapter.is_file(),
        "Windows update ZIP is missing Tailcat adapter"
    );
    ensure!(
        payload.config.is_file(),
        "Windows update ZIP is missing update configuration"
    );
    verify_config(&payload)?;
    Ok(payload)
}

pub(crate) fn verify_config(payload: &Payload) -> anyhow::Result<()> {
    let config: PackageConfig = serde_json::from_slice(&fs::read(&payload.config)?)
        .context("parsing Windows update configuration")?;
    ensure!(
        config.releases_url.starts_with("https://"),
        "update feed must use HTTPS"
    );
    ensure!(
        config.companion.file == ADAPTER,
        "unexpected Windows companion name"
    );
    ensure!(
        valid_digest(&config.companion.sha256),
        "invalid Tailcat adapter checksum"
    );
    verify_digest(&payload.adapter, &config.companion.sha256)
        .context("Tailcat adapter checksum mismatch")
}

pub(crate) fn verify_digest(path: &Path, expected: &str) -> anyhow::Result<()> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected),
        "checksum mismatch"
    );
    Ok(())
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn safe_name(name: &str) -> bool {
    let path = name.strip_suffix('/').unwrap_or(name);
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains(':')
        && !path.starts_with('/')
        && !path.ends_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn allowed(name: &str, directory: bool) -> bool {
    if directory {
        return matches!(
            name.trim_end_matches('/'),
            "licenses"
                | "licenses/fonts"
                | "licenses/tailcat"
                | "licenses/tailcat/go"
                | "licenses/tailcat/modules"
        );
    }
    matches!(
        name,
        APP | ADAPTER
            | CONFIG
            | "LICENSE"
            | "THIRD_PARTY_NOTICES.md"
            | "licenses/fonts/Geist-OFL.txt"
            | "licenses/fonts/THIRD_PARTY_NOTICES.md"
            | "licenses/tailcat/README.md"
            | "licenses/tailcat/DEPENDENCIES.txt"
            | "licenses/tailcat/go/LICENSE.txt"
            | "licenses/tailcat/go/PATENTS.txt"
    ) || allowed_tailcat_module_license(name)
}

fn allowed_tailcat_module_license(name: &str) -> bool {
    name.strip_prefix("licenses/tailcat/modules/")
        .is_some_and(|leaf| {
            !leaf.is_empty()
                && !leaf.contains('/')
                && leaf.ends_with(".txt")
                && leaf
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

/// Restore every old package member after an interrupted helper transaction.
/// The pending marker is removed only after all available backups are restored.
pub(crate) fn recover_pending(directory: &Path) -> anyhow::Result<()> {
    if !directory.join(PENDING).exists() {
        return Ok(());
    }
    for (installed, backup, incoming) in [
        (APP, BACKUP_APP, INCOMING_APP),
        (ADAPTER, BACKUP_ADAPTER, INCOMING_ADAPTER),
        (CONFIG, BACKUP_CONFIG, INCOMING_CONFIG),
    ] {
        let backup = directory.join(backup);
        if backup.exists() {
            let destination = directory.join(installed);
            let _ = fs::remove_file(&destination);
            fs::rename(&backup, destination).context("restoring interrupted Windows update")?;
        }
        let _ = fs::remove_file(directory.join(incoming));
    }
    fs::remove_file(directory.join(PENDING))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::FileOptions;

    fn archive(entries: &[(&str, &[u8], Option<u32>)]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(file.reopen().unwrap());
        for (name, bytes, mode) in entries {
            let mut options = FileOptions::default();
            if let Some(mode) = mode {
                options = options.unix_permissions(*mode);
            }
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        file
    }

    fn valid_entries() -> Vec<(&'static str, Vec<u8>, Option<u32>)> {
        let adapter = b"adapter".to_vec();
        let digest = format!("{:x}", Sha256::digest(&adapter));
        let config = format!(r#"{{"releases_url":"https://example.test/","companion":{{"file":"kratos-tailcat.exe","sha256":"{digest}"}}}}"#).into_bytes();
        vec![
            (APP, b"app".to_vec(), None),
            (ADAPTER, adapter, None),
            (CONFIG, config, None),
            (
                "licenses/tailcat/README.md",
                b"bundle readme".to_vec(),
                None,
            ),
            (
                "licenses/tailcat/DEPENDENCIES.txt",
                b"module\tversion\tlicense_files\n".to_vec(),
                None,
            ),
            (
                "licenses/tailcat/go/LICENSE.txt",
                b"Go license".to_vec(),
                None,
            ),
            (
                "licenses/tailcat/go/PATENTS.txt",
                b"Go patents".to_vec(),
                None,
            ),
            (
                "licenses/tailcat/modules/028-tailcat-a6e65ee922f1-LICENSE.txt",
                b"Tailcat license".to_vec(),
                None,
            ),
        ]
    }

    fn make(entries: &[(&str, Vec<u8>, Option<u32>)]) -> tempfile::NamedTempFile {
        let refs: Vec<_> = entries
            .iter()
            .map(|(n, b, m)| (*n, b.as_slice(), *m))
            .collect();
        archive(&refs)
    }

    #[test]
    fn extracts_expected_payload() {
        let zip = make(&valid_entries());
        let out = tempfile::tempdir().unwrap();
        let payload = extract(zip.path(), out.path()).unwrap();
        assert_eq!(fs::read(payload.app).unwrap(), b"app");
        assert_eq!(fs::read(payload.adapter).unwrap(), b"adapter");

        assert_eq!(
            fs::read(
                out.path()
                    .join("licenses/tailcat/modules/028-tailcat-a6e65ee922f1-LICENSE.txt")
            )
            .unwrap(),
            b"Tailcat license"
        );
    }

    #[test]
    fn rejects_traversal_absolute_backslash_duplicates_and_untrusted_files() {
        for name in [
            "../evil",
            "/evil",
            "dir\\evil",
            "C:/evil",
            "evil.dll",
            "licenses/fonts/evil.dll",
            "licenses/tailcat/evil.txt",
            "licenses/tailcat/go/NOTICE.txt",
            "licenses/tailcat/modules/evil.exe",
            "licenses/tailcat/modules/evil.dll",
            "licenses/tailcat/modules/evil.so",
            "licenses/tailcat/modules/evil.dylib",
            "licenses/tailcat/modules/notice.md",
            "licenses/tailcat/modules/deeper/LICENSE.txt",
            "sub/kratos.exe",
        ] {
            let zip = archive(&[(name, b"bad", None)]);
            assert!(
                extract(zip.path(), tempfile::tempdir().unwrap().path()).is_err(),
                "accepted {name}"
            );
        }
        let zip = archive(&[(APP, b"one", None), (APP, b"two", None)]);
        assert!(extract(zip.path(), tempfile::tempdir().unwrap().path()).is_err());
    }

    #[test]
    fn rejects_symlink_and_missing_or_corrupt_adapter() {
        let zip = archive(&[(ADAPTER, b"target", Some(0o120777))]);
        assert!(extract(zip.path(), tempfile::tempdir().unwrap().path()).is_err());

        let zip = archive(&[(
            "licenses/tailcat/modules/028-tailcat-LICENSE.txt",
            b"target",
            Some(0o120777),
        )]);
        assert!(extract(zip.path(), tempfile::tempdir().unwrap().path()).is_err());
        let mut missing = valid_entries();
        missing.retain(|(name, _, _)| *name != ADAPTER);
        assert!(extract(make(&missing).path(), tempfile::tempdir().unwrap().path()).is_err());
        let mut corrupt = valid_entries();
        corrupt
            .iter_mut()
            .find(|(n, _, _)| *n == ADAPTER)
            .unwrap()
            .1 = b"changed".to_vec();
        assert!(extract(make(&corrupt).path(), tempfile::tempdir().unwrap().path()).is_err());
    }

    #[test]
    fn interrupted_transaction_restores_app_adapter_and_config() {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            (APP, b"new-app".as_slice()),
            (ADAPTER, b"new-adapter"),
            (CONFIG, b"new-config"),
        ] {
            fs::write(dir.path().join(name), value).unwrap();
        }
        for (backup, value) in [
            (BACKUP_APP, b"old-app".as_slice()),
            (BACKUP_ADAPTER, b"old-adapter"),
            (BACKUP_CONFIG, b"old-config"),
        ] {
            fs::write(dir.path().join(backup), value).unwrap();
        }
        fs::write(dir.path().join(PENDING), b"stage").unwrap();
        recover_pending(dir.path()).unwrap();
        assert_eq!(fs::read(dir.path().join(APP)).unwrap(), b"old-app");
        assert_eq!(fs::read(dir.path().join(ADAPTER)).unwrap(), b"old-adapter");
        assert_eq!(fs::read(dir.path().join(CONFIG)).unwrap(), b"old-config");
        assert!(!dir.path().join(PENDING).exists());
    }
}
