//! Atomic updates for the portable Windows ZIP package.

use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::windows_archive::{
    self, ADAPTER, APP, CONFIG, INCOMING_ADAPTER, INCOMING_APP, INCOMING_CONFIG, PENDING, Payload,
};

const ARCHIVE: &str = "update.zip";
const ARCHIVE_DIGEST: &str = "archive.sha256";
const PAYLOAD_DIR: &str = "payload";
const STAGE_MARKER: &str = ".kratos-update-stage";
const HELPER_PREFIX: &str = ".kratos-update-helper-";

#[derive(serde::Deserialize)]
struct Config {
    releases_url: String,
}

pub(super) fn is_managed(exe: &Path) -> bool {
    if !exe.file_name().is_some_and(|name| name == APP) {
        return false;
    }
    let Some(directory) = exe.parent() else {
        return false;
    };
    // A killed helper leaves the pending marker until every old file has been
    // restored. Recover before checking package completeness because the
    // interrupted transaction itself may have temporarily displaced a file.
    let _ = windows_archive::recover_pending(directory);
    directory.join(CONFIG).is_file() && directory.join(ADAPTER).is_file()
}

pub(super) fn release_url() -> anyhow::Result<Option<String>> {
    let exe = std::env::current_exe()?;
    if !is_managed(&exe) {
        return Ok(None);
    }
    let config: Config = serde_json::from_slice(&std::fs::read(exe.with_file_name(CONFIG))?)
        .context("reading Windows update configuration")?;
    super::validate_release_override(&config.releases_url).map(Some)
}

/// The complete portable package is the update unit. Tailcat deliberately has
/// no wire/API stability promise, so updating only kratos.exe is unsafe.
pub fn artifact(version: &str) -> String {
    format!("kratos-{version}-windows-{}.zip", std::env::consts::ARCH)
}

pub async fn stage(
    edge_url: &str,
    manifest: &super::Manifest,
    directory: &Path,
) -> anyhow::Result<PathBuf> {
    let base = super::release_base(edge_url)?;
    stage_from_base(&base, manifest, directory).await
}

async fn stage_from_base(
    base: &str,
    manifest: &super::Manifest,
    directory: &Path,
) -> anyhow::Result<PathBuf> {
    ensure!(
        manifest
            .version
            .split('.')
            .all(|part| { !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) }),
        "invalid Windows release version"
    );
    let file = artifact(&manifest.version);
    let expected = manifest
        .files
        .get(&file)
        .and_then(|meta| meta.sha256.as_deref())
        .context("Windows ZIP updates require a SHA-256 checksum")?;
    ensure!(
        windows_archive::valid_digest(expected),
        "invalid Windows ZIP checksum"
    );

    let temporary = tempfile::Builder::new()
        .prefix(".kratos-update-")
        .tempdir_in(directory)?;
    let archive = temporary.path().join(ARCHIVE);
    super::download_release_file_from_base(base, manifest, &file, &archive).await?;
    std::fs::write(temporary.path().join(ARCHIVE_DIGEST), expected)?;
    windows_archive::verify_digest(&archive, expected).context("verifying Windows update ZIP")?;
    let payload = windows_archive::extract(&archive, &temporary.path().join(PAYLOAD_DIR))?;
    verify_app_version(&payload.app, &manifest.version).await?;
    std::fs::write(temporary.path().join(STAGE_MARKER), &manifest.version)?;
    let path = temporary.path().to_owned();
    let _ = temporary.keep();
    Ok(path)
}

async fn verify_app_version(app: &Path, version: &str) -> anyhow::Result<()> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(app)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("staged executable version check timed out")??;
    ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == format!("kratos {version}"),
        "staged executable has the wrong version or cannot run"
    );
    Ok(())
}

fn verify_stage(stage: &Path) -> anyhow::Result<Payload> {
    ensure!(
        stage.join(STAGE_MARKER).is_file(),
        "untrusted Windows update stage"
    );
    let expected = std::fs::read_to_string(stage.join(ARCHIVE_DIGEST))?;
    ensure!(
        windows_archive::valid_digest(expected.trim()),
        "invalid staged ZIP checksum"
    );
    windows_archive::verify_digest(&stage.join(ARCHIVE), expected.trim())
        .context("staged Windows ZIP checksum mismatch")?;
    windows_archive::validate(&stage.join(PAYLOAD_DIR))
}

/// Prepare all three installed files, then launch a private PowerShell helper.
/// The helper waits for this process to exit before replacing package members;
/// if a Tailcat child still locks the adapter, replacement fails and the helper
/// restores every old file. No new app launches until every replace succeeds.
pub fn apply(stage: &Path, directory: &Path, relaunch: bool) -> anyhow::Result<()> {
    let installed = directory.join(APP);
    ensure!(
        std::env::current_exe()?.canonicalize()? == installed.canonicalize()?,
        "update must run from its installation"
    );
    prepare_and_spawn(stage, directory, relaunch, std::process::id())
}

fn prepare_and_spawn(
    stage: &Path,
    directory: &Path,
    relaunch: bool,
    pid: u32,
) -> anyhow::Result<()> {
    windows_archive::recover_pending(directory)?;
    let payload = verify_stage(stage)?;
    for name in [APP, ADAPTER, CONFIG] {
        ensure!(
            directory.join(name).is_file(),
            "portable installation is incomplete"
        );
    }
    let mappings = [
        (&payload.app, directory.join(INCOMING_APP)),
        (&payload.adapter, directory.join(INCOMING_ADAPTER)),
        (&payload.config, directory.join(INCOMING_CONFIG)),
    ];
    for (source, incoming) in &mappings {
        let _ = std::fs::remove_file(incoming);
        std::fs::copy(source, incoming).context("preparing Windows update file")?;
        let digest = digest(source)?;
        windows_archive::verify_digest(incoming, &digest)
            .context("verifying prepared Windows update file")?;
    }
    std::fs::write(directory.join(PENDING), stage.to_string_lossy().as_bytes())?;
    let helper = directory.join(format!("{HELPER_PREFIX}{pid}.ps1"));
    std::fs::write(&helper, HELPER_SCRIPT)?;
    let spawn = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&helper)
        .arg(pid.to_string())
        .arg(directory)
        .arg(stage)
        .arg(if relaunch { "1" } else { "0" })
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(error) = spawn {
        let _ = std::fs::remove_file(directory.join(PENDING));
        let _ = std::fs::remove_file(&helper);
        for (_, incoming) in &mappings {
            let _ = std::fs::remove_file(incoming);
        }
        return Err(error).context("launching Windows update helper");
    }
    Ok(())
}

fn digest(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn wait_for_exit(pid: u32) -> anyhow::Result<()> {
    ensure!(pid != std::process::id(), "cannot wait for own process");
    let result = wait_for_exit_impl(pid);
    if let Ok(exe) = std::env::current_exe()
        && let Some(directory) = exe.parent()
    {
        let _ = windows_archive::recover_pending(directory);
    }
    result
}

fn wait_for_exit_impl(pid: u32) -> anyhow::Result<()> {
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(());
        }
        return Err(error).context("opening previous process");
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    if unsafe { WaitForSingleObject(process.as_raw_handle(), 60000) } != WAIT_OBJECT_0 {
        bail!("previous process did not exit within 60 seconds");
    }
    Ok(())
}

const HELPER_SCRIPT: &str = r#"param([uint32]$PidToWait,[string]$Install,[string]$Stage,[string]$Relaunch)
$ErrorActionPreference = 'Stop'
$names = @(
  @('kratos.exe','.kratos-update-incoming-kratos.exe','.kratos-update-backup-kratos.exe'),
  @('kratos-tailcat.exe','.kratos-update-incoming-kratos-tailcat.exe','.kratos-update-backup-kratos-tailcat.exe'),
  @('kratos-update.json','.kratos-update-incoming-config.json','.kratos-update-backup-config.json')
)
try { Wait-Process -Id $PidToWait -ErrorAction SilentlyContinue } catch {}
$done = @()
try {
  foreach ($n in $names) {
    $dst = Join-Path $Install $n[0]; $incoming = Join-Path $Install $n[1]; $backup = Join-Path $Install $n[2]
    Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    [IO.File]::Replace($incoming, $dst, $backup, $true)
    $done += ,$n
  }
  Remove-Item -LiteralPath (Join-Path $Install '.kratos-update-pending') -Force
  foreach ($n in $names) { Remove-Item -LiteralPath (Join-Path $Install $n[2]) -Force -ErrorAction SilentlyContinue }
  if (Test-Path -LiteralPath (Join-Path $Stage '.kratos-update-stage')) { Remove-Item -LiteralPath $Stage -Recurse -Force }
  if ($Relaunch -eq '1') { Start-Process -FilePath (Join-Path $Install 'kratos.exe') -ArgumentList @('--wait-for-exit', "$PidToWait") }
} catch {
  [array]::Reverse($done)
  foreach ($n in $done) {
    $dst = Join-Path $Install $n[0]; $backup = Join-Path $Install $n[2]
    Remove-Item -LiteralPath $dst -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $backup) { Move-Item -LiteralPath $backup -Destination $dst -Force }
  }
  foreach ($n in $names) { Remove-Item -LiteralPath (Join-Path $Install $n[1]) -Force -ErrorAction SilentlyContinue }
  Remove-Item -LiteralPath (Join-Path $Install '.kratos-update-pending') -Force -ErrorAction SilentlyContinue
  exit 1
} finally {
  Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_artifact_is_zip() {
        assert!(artifact("1.2.3").ends_with("-windows-x86_64.zip"));
    }

    #[test]
    fn portable_install_requires_adapter_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(APP);
        std::fs::write(&exe, b"app").unwrap();
        std::fs::write(dir.path().join(CONFIG), b"{}").unwrap();
        assert!(!is_managed(&exe));
        std::fs::write(dir.path().join(ADAPTER), b"adapter").unwrap();
        assert!(is_managed(&exe));
    }

    fn run_helper_fixture(missing_adapter: bool) -> (tempfile::TempDir, std::process::ExitStatus) {
        let install = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        for (name, value) in [
            (APP, "old-app"),
            (ADAPTER, "old-adapter"),
            (CONFIG, "old-config"),
        ] {
            std::fs::write(install.path().join(name), value).unwrap();
        }
        for (name, value) in [
            (INCOMING_APP, "new-app"),
            (INCOMING_ADAPTER, "new-adapter"),
            (INCOMING_CONFIG, "new-config"),
        ] {
            if !(missing_adapter && name == INCOMING_ADAPTER) {
                std::fs::write(install.path().join(name), value).unwrap();
            }
        }
        std::fs::write(install.path().join(PENDING), b"stage").unwrap();
        std::fs::write(stage.path().join(STAGE_MARKER), b"1.2.3").unwrap();
        let script = install.path().join("helper-test.ps1");
        std::fs::write(&script, HELPER_SCRIPT).unwrap();
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&script)
            .arg(u32::MAX.to_string())
            .arg(install.path())
            .arg(stage.path())
            .arg("0")
            .status()
            .unwrap();
        (install, status)
    }

    #[test]
    fn helper_replaces_complete_package() {
        let (install, status) = run_helper_fixture(false);
        assert!(status.success());
        assert_eq!(std::fs::read(install.path().join(APP)).unwrap(), b"new-app");
        assert_eq!(
            std::fs::read(install.path().join(ADAPTER)).unwrap(),
            b"new-adapter"
        );
        assert_eq!(
            std::fs::read(install.path().join(CONFIG)).unwrap(),
            b"new-config"
        );
        assert!(!install.path().join(PENDING).exists());
    }

    #[test]
    fn helper_rolls_back_all_members_on_failure() {
        let (install, status) = run_helper_fixture(true);
        assert!(!status.success());
        assert_eq!(std::fs::read(install.path().join(APP)).unwrap(), b"old-app");
        assert_eq!(
            std::fs::read(install.path().join(ADAPTER)).unwrap(),
            b"old-adapter"
        );
        assert_eq!(
            std::fs::read(install.path().join(CONFIG)).unwrap(),
            b"old-config"
        );
        assert!(!install.path().join(PENDING).exists());
    }

    #[tokio::test]
    async fn missing_zip_checksum_fails_before_network_or_staging() {
        let manifest = super::super::Manifest {
            version: "1.2.3".into(),
            files: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let error = stage_from_base("http://127.0.0.1:1", &manifest, dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("SHA-256"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
