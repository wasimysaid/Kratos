//! kratos-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`kratos update`), and the UI
//! (the sidebar update strip + macOS bundle swap).
//!
//! Release layout (see `.github/workflows/release.yml` and `scripts/install.sh`):
//! versioned artifacts and a checksum manifest are attached to each GitHub
//! Release. The stable `releases/latest/download` URLs redirect to the newest
//! non-prerelease release, so update checks do not depend on the application
//! edge service.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.kratos/app/<ver>` + `current` symlink — the curl|sh
//!   installer): download the headless tarball into a new versioned dir, flip
//!   the symlink, restart the service. Same flow the installer script performs,
//!   natively.
//! - **MacApp** (running out of an app bundle): download the app tarball, swap the
//!   bundle directory, relaunch. Driven by the UI.
//! - **Unmanaged** (source builds, hand-copied binaries): report only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

#[cfg(windows)]
pub mod windows;

#[cfg(any(windows, test))]
mod windows_archive;

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Background check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Retry sooner after a failed check (offline boot, transient release-host error).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// First check waits out engine boot (room joins, doc re-sync).
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Stable feed for the newest public GitHub Release.
pub const DEFAULT_RELEASES_URL: &str =
    "https://github.com/wasimysaid/Kratos/releases/latest/download";

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// `manifest.json` attached to every GitHub Release by the release workflow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Artifact file name → required SHA-256 metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Artifact-name platform pair matching the packaging scripts. Unsupported
/// updater targets retain their real OS name rather than impersonating Linux.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

fn managed_updates_supported(os: &str) -> bool {
    matches!(os, "linux" | "macos")
}

fn require_managed_update_platform() -> anyhow::Result<()> {
    if !managed_updates_supported(std::env::consts::OS) {
        bail!(
            "managed updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

fn require_mac_app_update_platform() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        bail!(
            "macOS app updates are not supported on {}",
            std::env::consts::OS
        );
    }
    Ok(())
}

/// `kratos-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("kratos-{version}-{os}-{arch}.tar.gz")
}

/// `kratos-<ver>-macos-<arch>-app.tar.gz` — the macOS app update payload.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("kratos-{version}-macos-{arch}-app.tar.gz")
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer — malformed release metadata must
/// not trigger an update loop.
pub fn version_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let nums: Vec<u64> = v
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (!nums.is_empty()).then_some(nums)
    }
    match (parts(latest), parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Fetch the newest GitHub Release metadata.
///
/// The `edge_url` argument remains temporarily for caller API compatibility but
/// is deliberately not used to derive the release feed. Set
/// `KRATOS_RELEASES_URL` for an explicit mirror/test feed; Windows portable
/// packages can also carry an HTTPS feed in `kratos-update.json`.
pub async fn fetch_latest(edge_url: &str) -> anyhow::Result<Manifest> {
    let base = release_base(edge_url)?;
    fetch_latest_from_base(&base).await
}

async fn fetch_latest_from_base(base: &str) -> anyhow::Result<Manifest> {
    let manifest_url = format!("{base}/manifest.json");
    let manifest: Manifest = http_client()?
        .get(&manifest_url)
        .send()
        .await
        .with_context(|| format!("fetching {manifest_url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {manifest_url}"))?
        .json()
        .await
        .context("parsing manifest.json")?;
    ensure!(
        valid_version(&manifest.version),
        "manifest.json has an invalid version"
    );
    Ok(manifest)
}

fn valid_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let valid = parts
        .by_ref()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid && version.matches('.').count() == 2
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("kratos/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many update redirects");
            }
            if attempt.previous().iter().any(|url| url.scheme() == "https")
                && attempt.url().scheme() != "https"
            {
                return attempt.error("update redirect would downgrade HTTPS");
            }
            attempt.follow()
        }))
        .build()
        .context("building http client")
}

fn validate_release_override(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("invalid update feed URL")?;
    anyhow::ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "update feed must use HTTPS"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "update feed must be a base URL without credentials, query, or fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn release_base(edge_url: &str) -> anyhow::Result<String> {
    if let Ok(url) = std::env::var("KRATOS_RELEASES_URL")
        && !url.trim().is_empty()
    {
        return validate_release_override(&url);
    }
    #[cfg(windows)]
    if let Some(url) = windows::release_url()? {
        return Ok(url.trim_end_matches('/').to_owned());
    }
    Ok(DEFAULT_RELEASES_URL.to_owned())
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.kratos/app/<ver>/kratos` behind the `current` symlink
    /// (curl|sh installer / a previous `kratos update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Portable Windows package with an explicit update-feed configuration.
    #[cfg(windows)]
    WindowsPortable { directory: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

impl InstallKind {
    pub fn supports_desktop_update(&self) -> bool {
        match self {
            Self::MacApp { .. } => true,
            #[cfg(windows)]
            Self::WindowsPortable { .. } => true,
            _ => false,
        }
    }

    pub async fn stage_desktop(
        &self,
        edge_url: &str,
        manifest: &Manifest,
        data_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        match self {
            Self::MacApp { .. } => stage_mac_app(edge_url, manifest, data_dir).await,
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                windows::stage(edge_url, manifest, directory).await
            }
            _ => bail!("this installation does not support desktop updates"),
        }
    }

    /// Install and arrange a relaunch. The UI must quit after this succeeds.
    pub fn apply_desktop(&self, staged: &Path) -> anyhow::Result<()> {
        match self {
            Self::MacApp { bundle } => {
                apply_mac_app(staged, bundle)?;
                relaunch_app_after_exit(bundle);
                Ok(())
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => windows::apply(staged, directory, true),
            _ => bail!("this installation does not support desktop updates"),
        }
    }
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    detect_install_from_for_os(exe, home, std::env::consts::OS)
}

fn detect_install_from_for_os(exe: &Path, home: Option<&Path>, os: &str) -> InstallKind {
    #[cfg(windows)]
    if os == "windows" && windows::is_managed(exe) {
        return InstallKind::WindowsPortable {
            directory: exe.parent().unwrap().to_owned(),
        };
    }
    // Never interpret a coincidental Windows `%HOME%\.kratos\app` layout as
    // the Unix symlink-managed installation.
    if !managed_updates_supported(os) {
        return InstallKind::Unmanaged;
    }
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = home.join(".kratos").join("app");
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream a GitHub Release asset to `dest`, requiring and verifying its
/// manifest SHA-256. Writes through a `.partial` sidecar so an interrupted
/// download never leaves a plausible-looking artifact behind.
pub async fn download_release_file(
    edge_url: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let base = release_base(edge_url)?;
    download_release_file_from_base(&base, manifest, file, dest).await
}

async fn download_release_file_from_base(
    base: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    ensure!(
        Path::new(file).file_name().is_some_and(|name| name == file),
        "invalid release asset name"
    );
    let expected = manifest
        .files
        .get(file)
        .and_then(|meta| meta.sha256.as_deref())
        .context("release asset requires a SHA-256 checksum")?;
    ensure!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 checksum for {file}"
    );
    let url = format!("{base}/{file}");
    let partial = dest.with_extension("partial");
    let resp = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.context("writing download")?;
    }
    out.flush().await.ok();
    drop(out);
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        tokio::fs::remove_file(&partial).await.ok();
        bail!("checksum mismatch for {file}: expected {expected}, got {actual}");
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    edge_url: &str,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_managed_update_platform()?;
    let version = &manifest.version;
    let dest = app_root.join(version);
    if dest.join("kratos").exists() {
        return Ok(dest);
    }
    let file = headless_artifact(version);
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(edge_url, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("kratos").is_file() {
            bail!("tarball {file} did not contain a kratos binary");
        }
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_) if dest.join("kratos").exists() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("kratos").exists() {
            bail!("{} is not a staged install", target.display());
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        require_managed_update_platform()?;
        unreachable!("supported managed-update platforms are Unix")
    }
}

/// Restart the installed engine service (the same units `kratos daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary.
pub fn restart_service() -> anyhow::Result<()> {
    require_managed_update_platform()?;
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/sh.kratos.app")],
        )
    } else {
        run("systemctl", &["--user", "restart", "kratos.service"])
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Kratos.app`
/// (idempotent). Returns the staged bundle path.
pub async fn stage_mac_app(
    edge_url: &str,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_mac_app_update_platform()?;
    let version = &manifest.version;
    let dir = data_dir.join("updates").join(version);
    let staged = dir.join("Kratos.app");
    if staged.join("Contents/MacOS/kratos").exists() {
        return Ok(staged);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = mac_app_artifact(version);
    let tarball = dir.join(&file);
    download_release_file(edge_url, manifest, &file, &tarball).await?;
    run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&tarball).ok();
    if !staged.join("Contents/MacOS/kratos").exists() {
        bail!("app tarball {file} did not contain Kratos.app");
    }
    Ok(staged)
}

/// Swap the installed bundle for the staged one: `ditto` the staged copy next to
/// the target (metadata-preserving, cross-volume safe), then two renames — the
/// old bundle is restored if the second rename fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<()> {
    require_mac_app_update_platform()?;
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{name}.new-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&fresh);
    run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    )?;
    std::fs::rename(bundle, &old).context("moving the current app aside")?;
    if let Err(err) = std::fs::rename(&fresh, bundle) {
        let _ = std::fs::rename(&old, bundle);
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Detached relauncher: waits for THIS process to exit, then `open`s the bundle.
/// (Opening before exit would race the single-instance engine lock and the IPC
/// port.) The caller quits the app after this returns.
pub fn relaunch_app_after_exit(bundle: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id();
        let script = format!(
            "while /bin/kill -0 {pid} 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"{}\"",
            bundle.display()
        );
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = bundle;
}

// ---------------------------------------------------------------------------
// Engine-side checker
// ---------------------------------------------------------------------------

/// What the engine reports over the `UpdateStatus` stream. Version facts only —
/// download/apply progress is owned by whoever drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
        }
    }
}

/// `KRATOS_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("KRATOS_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Background release checker: polls the GitHub Release feed on a 6h cadence and
/// publishes [`UpdateStatus`] over a watch channel (the `UpdateStatus` RPC
/// stream). Managed installs with `KRATOS_AUTO_UPDATE` set stage + apply + service
/// restart on their own — but only in a quiet window: while `quiescent` reports
/// activity, the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    edge_url: String,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    check_tx: Arc<watch::Sender<u64>>,
    quiescent: Option<QuiescentCheck>,
    /// Flips to true exactly once; the check loop selects against it so
    /// cancellation lands at any await point (no tokio-util in this crate).
    shutdown_tx: Arc<watch::Sender<bool>>,
    check_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(edge_url: String, quiescent: Option<QuiescentCheck>) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let (check_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let updater = Self {
            edge_url,
            status_tx: Arc::new(status_tx),
            check_tx: Arc::new(check_tx),
            quiescent,
            shutdown_tx: Arc::new(shutdown_tx),
            check_task: Arc::new(std::sync::Mutex::new(None)),
        };
        let for_loop = updater.clone();
        let task = tokio::spawn(async move { for_loop.check_loop().await });
        *updater.check_task.lock().unwrap() = Some(task);
        updater
    }

    /// Stop the check loop and wait for it to exit — a replaced runtime must
    /// not keep polling the release feed (or auto-applying) in the background.
    /// Idempotent, and callable from any clone.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let task = self
            .check_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    /// Wake the release checker immediately, for example when authentication
    /// recovers after the process started offline.
    pub fn check_now(&self) {
        self.check_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn check_loop(&self) {
        let mut shutdown = self.shutdown_tx.subscribe();
        // Shutdown must cut the loop at ANY await point — including mid
        // `check_once()` / `auto_apply_when_idle()` HTTP — so the whole body
        // races the flag rather than checking it between iterations.
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => {}
            _ = async {
                let mut checks = self.check_tx.subscribe();
                tokio::select! {
                    _ = tokio::time::sleep(CHECK_INITIAL_DELAY) => {}
                    _ = checks.changed() => {}
                }
                loop {
                    let ok = self.check_once().await;
                    if ok
                        && self.status_tx.borrow().update_available
                        && auto_update_enabled()
                        && let InstallKind::Managed { .. } = detect_install()
                    {
                        self.auto_apply_when_idle().await;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }) => {}
                        _ = checks.changed() => {}
                    }
                }
            } => {}
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and reuses the staged dir, keeping the
    /// idle→restart gap to well under a second.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            match fetch_latest(&self.edge_url).await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    if let Err(err) = stage_headless(&self.edge_url, &manifest, &app_root).await {
                        tracing::warn!(error = %err, "auto-update staging failed");
                        return;
                    }
                }
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update staging fetch failed");
                    return;
                }
            }
        }
        let mut deferred = false;
        while !self.quiescent_now() {
            if !deferred {
                deferred = true;
                tracing::info!("auto-update deferred: sessions or terminals active");
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
        match self.apply().await {
            Ok(version) => {
                tracing::info!(%version, "auto-update applied; service restarting")
            }
            Err(err) => tracing::warn!(error = %err, "auto-update failed"),
        }
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match fetch_latest(&self.edge_url).await {
            Ok(manifest) => {
                let status = UpdateStatus {
                    current_version: current_version().to_string(),
                    update_available: version_newer(&manifest.version, current_version()),
                    latest_version: Some(manifest.version),
                    checked_at: Some(now_ms()),
                    error: None,
                };
                if status.update_available {
                    tracing::info!(
                        latest = status.latest_version.as_deref().unwrap_or(""),
                        current = %status.current_version,
                        "update available"
                    );
                }
                self.status_tx.send_replace(status);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = fetch_latest(&self.edge_url).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        stage_headless(&self.edge_url, &manifest, &app_root).await?;
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(manifest.version)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_feed_override_requires_an_https_base_url() {
        assert_eq!(
            validate_release_override(" https://example.com/releases/ ").unwrap(),
            "https://example.com/releases"
        );
        for url in [
            "http://example.com/releases",
            "file:///tmp/update",
            "https://user:password@example.com",
            "https://example.com?feed=x",
            "https://example.com/#fragment",
        ] {
            assert!(validate_release_override(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/home/u/.kratos/app/0.1.1/kratos"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.kratos/app")
            }
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/Applications/Kratos.app/Contents/MacOS/kratos"),
                Some(Path::new("/Users/u")),
                "macos",
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Kratos.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from_for_os(Path::new("/tmp/foo.app/kratos"), None, "macos"),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/src/target/release/kratos"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Unmanaged
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("kratos-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("kratos-0.2.0-{os}-{arch}.tar.gz")
        );
        assert!(mac_app_artifact("0.2.0").ends_with("-app.tar.gz"));
    }

    #[test]
    fn default_feed_is_github_and_does_not_derive_from_edge() {
        if std::env::var_os("KRATOS_RELEASES_URL").is_none() {
            assert_eq!(
                release_base("https://legacy.example.invalid").unwrap(),
                DEFAULT_RELEASES_URL
            );
        }
    }

    #[tokio::test]
    async fn downloads_require_a_well_formed_checksum_before_network_io() {
        let tmp = tempfile::tempdir().unwrap();
        let file = "kratos-1.2.3-linux-x86_64.tar.gz";
        let mut manifest = Manifest {
            version: "1.2.3".into(),
            files: BTreeMap::new(),
        };
        let error = download_release_file_from_base(
            "http://127.0.0.1:1",
            &manifest,
            file,
            &tmp.path().join(file),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("requires a SHA-256"));

        manifest.files.insert(
            file.into(),
            FileMeta {
                sha256: Some("not-a-digest".into()),
            },
        );
        let error = download_release_file_from_base(
            "http://127.0.0.1:1",
            &manifest,
            file,
            &tmp.path().join(file),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invalid SHA-256"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_is_not_misclassified_as_linux() {
        assert_eq!(platform_key().0, "windows");
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_is_always_unmanaged() {
        assert_eq!(
            detect_install_from(
                Path::new(r"C:\Users\u\.kratos\app\0.2.0\kratos.exe"),
                Some(Path::new(r"C:\Users\u")),
            ),
            InstallKind::Unmanaged
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_rejects_platform_specific_updates_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "9.9.9".into(),
            files: BTreeMap::new(),
        };
        let app_root = tmp.path().join("app");
        let data_dir = tmp.path().join("data");

        let managed_err = stage_headless("http://127.0.0.1:1", &manifest, &app_root)
            .await
            .unwrap_err();
        assert!(managed_err.to_string().contains("not supported on windows"));
        assert!(!app_root.exists(), "managed staging must not touch disk");

        let mac_err = stage_mac_app("http://127.0.0.1:1", &manifest, &data_dir)
            .await
            .unwrap_err();
        assert!(mac_err.to_string().contains("not supported on windows"));
        assert!(!data_dir.exists(), "macOS staging must not touch disk");

        assert!(
            apply_headless(&app_root, &manifest.version)
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            apply_mac_app(&data_dir.join("Kratos.app"), &data_dir.join("Installed.app"))
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            restart_service()
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
    }

    #[test]
    fn manifest_parses_with_and_without_files() {
        let full: Manifest = serde_json::from_str(
            r#"{"version":"0.1.1","files":{"kratos-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(
            full.files["kratos-0.1.1-linux-x86_64.tar.gz"]
                .sha256
                .as_deref(),
            Some("abc")
        );
        let bare: Manifest = serde_json::from_str(r#"{"version":"0.1.1"}"#).unwrap();
        assert!(bare.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("kratos"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }
}
