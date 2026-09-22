//! zeron — headed by default; `zeron headless` runs the engine alone. Both start
//! local-only without credentials. Pairing and `zeron logout` select the
//! profile used by the next engine start without mutating a live runtime.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod auth_cli;

mod backup_cli;
mod daemon;
mod paths;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "zeron",
    version,
    about = "Multi-device controller for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Open a Zeron conversation URL.
    #[arg(value_name = "URL")]
    open_url: Option<String>,
    #[cfg(windows)]
    #[arg(long, hide = true)]
    wait_for_exit: Option<u32>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (local-only unless a saved session enables sync).
    Headless,
    /// Pair using an invitation from stdin or a private file (never a command-line secret).
    Pair {
        #[arg(long)]
        code_file: Option<std::path::PathBuf>,
    },
    /// Initialize or manage this installation's durable sync peer.
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    /// Remove the saved session and return to local-only on the next start.
    Logout,
    /// Show workspace mode, optional auth, and engine status.
    Status,
    /// Live sync introspection from the running engine: per-room connection
    /// state, last pushed-frame/ack ages, rejoin/probe/resync counters.
    Sync,
    #[cfg(target_os = "linux")]
    /// Trigger an Appshot in the running headed instance (desktop shortcut fallback).
    Appshot,
    /// Serve the Zeron MCP (Model Context Protocol) server on stdin/stdout,
    /// proxying to the running engine's IPC. Agents use it to create, read,
    /// and message chats. Logs go to stderr; stdout is the protocol.
    Mcp,
    /// Manage `zeron headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum PeerCommand {
    /// Create a durable peer. Keep it running for offline device catch-up.
    Init {
        #[arg(long, default_value = "")]
        name: String,
        /// Use an owned DERP map instead of the rate-limited public relay service.
        #[arg(long)]
        derp_map: Option<String>,
    },
    /// Export a short-lived one-use invitation to stdout or a new private file.
    Invite {
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },
    /// List profile-bound trusted devices.
    Devices,
    /// Revoke a trusted device and close its active connections.
    Revoke { device_id: String },
    /// Create and export a complete private hosted-peer backup generation.
    Backup {
        /// Secure directory to create or use; receives <generation UUID>/.
        #[arg(long)]
        output_dir: std::path::PathBuf,
    },
    /// Restore one generation into a new, absent data directory.
    Restore {
        /// Complete generation directory containing manifest.json.
        #[arg(long)]
        generation: std::path::PathBuf,
        /// New data directory to create; existing paths are never merged.
        #[arg(long)]
        destination: std::path::PathBuf,
        /// Accept rollback of trusted-device and revocation state to backup time.
        #[arg(long)]
        acknowledge_trust_rollback: bool,
    },

    /// Show peer/profile state without printing secret addresses or credentials.
    Status,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures ZERON_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

/// mimalloc, macOS only: libmalloc never returns the streaming churn's
/// high-water pages, so transient allocation became permanent RSS
/// (docs/memory-plan.md §1). Pinned to mimalloc v2 in the workspace manifest —
/// the crate's default v3 has the same pathology (churn retained as permanent
/// RSS, ~6x glibc's growth on identical workloads, no idle recovery). Linux
/// measured flat on glibc, so it keeps the system allocator.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    attach_parent_console();
    let cli = Cli::parse();
    reject_removed_commands(&cli)?;
    #[cfg(windows)]
    if let Some(pid) = cli.wait_for_exit {
        zeron_update::windows::wait_for_exit(pid)?;
    }
    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left every sync
    // wedge report ("stale until restart") with zero diagnostics even though
    // the engine logs the exact failure line. One file per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // `zeron mcp` owns stdout for the protocol: a single log line on it
        // would corrupt the JSON-RPC stream, so its diagnostics go to stderr.
        if matches!(&cli.command, Some(Command::Mcp)) {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::io::stderr),
                )
                .init();
        } else {
            let registry = tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer());
            match log_file {
                Some(file) => registry
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(std::sync::Arc::new(file)),
                    )
                    .init(),
                None => registry.init(),
            }
        }
    }

    if long_running {
        // Finder launches have no visible stderr. Mirror the panic location
        // and backtrace into the same rotating log as engine diagnostics.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(panic = %info,
                backtrace = %std::backtrace::Backtrace::force_capture(),
                "application panic");
            default_hook(info);
        }));
    }

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = zeron_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Pair { code_file }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::pair(
                engine_config_from_env(),
                code_file.as_deref(),
            ))
        }
        Some(Command::Peer { command }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            let config = engine_config_from_env();
            runtime.block_on(async move {
                match command {
                    PeerCommand::Init { name, derp_map } => {
                        auth_cli::initialize(config, name, derp_map).await
                    }
                    PeerCommand::Invite { output } => {
                        auth_cli::invite(config, output.as_deref()).await
                    }
                    PeerCommand::Devices => auth_cli::devices(config).await,
                    PeerCommand::Revoke { device_id } => auth_cli::revoke(config, device_id).await,
                    PeerCommand::Backup { output_dir } => {
                        backup_cli::backup(&config, &output_dir)?;
                        Ok(())
                    }
                    PeerCommand::Restore {
                        generation,
                        destination,
                        acknowledge_trust_rollback,
                    } => backup_cli::restore(&generation, &destination, acknowledge_trust_rollback),
                    PeerCommand::Status => auth_cli::status(config).await,
                }
            })
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Sync) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(sync_cli(engine_config_from_env().ipc_port))
        }
        Some(Command::Mcp) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(zeron_mcp::run(zeron_mcp::McpConfig::from_env()))
        }
        #[cfg(target_os = "linux")]
        Some(Command::Appshot) => {
            zeron_ui::appshots::request_running_appshot(&engine_config_from_env().data_dir)
                .map_err(anyhow::Error::msg)
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update("", check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: the UI probes ZERON_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            zeron_ui::run_app(zeron_ui::UiConfig {
                data_dir: paths::data_dir(),
                ipc_port: std::env::var("ZERON_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                default_harness: zeron_ui::HarnessId::ClaudeCode,
                initial_url: cli.open_url,
            });
            Ok(())
        }
    }
}

fn reject_removed_commands(cli: &Cli) -> anyhow::Result<()> {
    if cli.command.is_none() && cli.open_url.as_deref() == Some("migrate") {
        anyhow::bail!("unrecognized subcommand 'migrate'");
    }
    Ok(())
}

#[cfg(test)]
mod removed_command_tests {
    use super::*;

    #[test]
    fn migrate_is_rejected_after_cli_removal() {
        let cli = Cli::try_parse_from(["zeron", "migrate"]).expect("positional parser");
        assert!(reject_removed_commands(&cli).is_err());
    }
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };

    // The GUI subsystem prevents Explorer from creating a console at startup.
    // Reuse an existing parent's console for CLI output and cargo run, without
    // allocating one. Attach before Clap so help and argument errors work too.
    // Preserve redirected pipes/files: attaching may replace standard handles.
    unsafe {
        let saved = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id)));
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            for (id, handle) in saved {
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    SetStdHandle(id, handle);
                }
            }
        }
    }
}

/// One local configuration for UI, daemon, and pairing. Connectivity and profile
/// selection come exclusively from the installation's saved trusted-peer state.
fn engine_config_from_env() -> zeron_engine::EngineConfig {
    zeron_engine::EngineConfig {
        data_dir: paths::data_dir(),
        ipc_port: std::env::var("ZERON_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
    }
}

/// `ZERON_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> zeron_engine::HarnessId {
    match std::env::var("ZERON_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => zeron_engine::HarnessId::Mock,
        Ok("codex") => zeron_engine::HarnessId::Codex,
        Ok("cursor") => zeron_engine::HarnessId::Cursor,
        Ok("devin") => zeron_engine::HarnessId::Devin,
        Ok("grok") => zeron_engine::HarnessId::Grok,
        Ok("hermes") => zeron_engine::HarnessId::Hermes,
        Ok("pi") => zeron_engine::HarnessId::Pi,
        Ok("mimir") => zeron_engine::HarnessId::Mimir,
        Ok("antigravity") => zeron_engine::HarnessId::Antigravity,
        _ => zeron_engine::HarnessId::ClaudeCode,
    }
}

/// `zeron sync`: dial the running engine's IPC and print per-room sync state.
/// The introspection surface every 2026-08 incident was missing — "is this
/// device's workspace room actually receiving?" as a one-liner.
async fn sync_cli(ipc_port: u16) -> anyhow::Result<()> {
    let client = zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({e}) — is zeron running?")
        })?;
    let status = client
        .call(zeron_rpc::methods::SYNC_STATUS, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("SyncStatus failed: {e}"))?;
    let now = status.get("nowMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let age = |ms: i64| -> String {
        if ms <= 0 {
            return "never".into();
        }
        let s = (now - ms).max(0) / 1000;
        if s >= 3600 {
            format!("{}h{}m ago", s / 3600, (s % 3600) / 60)
        } else if s >= 60 {
            format!("{}m{}s ago", s / 60, s % 60)
        } else {
            format!("{s}s ago")
        }
    };
    let room_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        // REJECTED is loud and only shown when nonzero: rejected writes with
        // a fresh-looking room is exactly the latched-session wedge
        // (2026-08-04) this readout previously masked.
        let rejected = get("rejected");
        format!(
            "{} pushed {} · acked {} · rejoins {} probes {} resyncs {} drops {}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            age(get("lastPushedMs")),
            age(get("lastAckMs")),
            get("rejoins"),
            get("probes"),
            get("fullResyncs"),
            get("disconnects"),
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    println!(
        "Device:    {}",
        status
            .get("deviceId")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "Workspace: {}",
        room_line(status.get("workspace").filter(|v| !v.is_null()))
    );
    let chats = status
        .get("chats")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if chats.is_empty() {
        println!("Chats:     none open");
    }
    // Chat rooms speak chat2: cursor/head tell "am I caught up?", pending
    // tells "did my writes leave?", resets/rejected are the loud tells.
    let chat_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let resets = get("serverResets");
        let rejected = get("rejected");
        format!(
            "{} cursor {}/{} · pending {} · rows {} ({}KB) · rejoins {} drops {}{}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            get("cursor"),
            get("headSeq"),
            get("pendingPushes"),
            get("rowCount"),
            get("rowBytes") / 1024,
            get("rejoins"),
            get("disconnects"),
            if resets > 0 {
                format!(" RESETS {resets}")
            } else {
                String::new()
            },
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    for chat in &chats {
        println!(
            "Chat {}: {}",
            chat.get("chatId")
                .and_then(|v| v.as_str())
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("?"),
            chat_line(chat.get("room").filter(|v| !v.is_null()))
        );
    }
    Ok(())
}

/// `{data_dir}/logs/zeron-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
///
/// The returned file holds an exclusive `flock` for the process lifetime:
/// rotate-on-launch is only safe when nothing is still WRITING the current
/// file. On 2026-08-04 a dev build launched twice next to the running
/// installed app — the first rename put the daemon's live log at `.old`, the
/// second unlinked it entirely, and the daemon spent the rest of the incident
/// logging to an orphaned inode (an entire day of sync diagnostics gone at
/// the exact moment they were needed). A launch that finds the canonical file
/// locked logs to `zeron-{mode}.{pid}.log` instead; the next lock-holding
/// launch sweeps pid-suffixed files older than a week.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = paths::data_dir().join("logs");
    open_log_file_in(&dir, mode)
}

/// Dir-parameterized body of [`open_log_file`] (unit-testable without env).
fn open_log_file_in(dir: &std::path::Path, mode: &str) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("zeron-{mode}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Probe the CURRENT inode for a live writer before touching it.
        let preexisting = path.exists();
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { libc::flock(existing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // A live process owns the canonical log — leave it alone.
            return std::fs::File::create(
                dir.join(format!("zeron-{mode}.{}.log", std::process::id())),
            )
            .ok();
        }
        // No live writer: rotate, create fresh, and lock it as ours. (The
        // probe's flock dies with `existing`; a first-ever launch has nothing
        // to rotate — the probe itself created the empty file.)
        drop(existing);
        if preexisting {
            let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        }
        let file = std::fs::File::create(&path).ok()?;
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        sweep_stale_pid_logs(dir, mode);
        Some(file)
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        std::fs::File::create(&path).ok()
    }
}

#[cfg(all(test, unix))]
mod log_file_tests {
    use super::open_log_file_in;

    #[test]
    fn second_launch_never_rotates_a_live_processes_log() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // First launch owns the canonical file and keeps writing.
        let first = open_log_file_in(dir, "headed").expect("first log");
        assert!(dir.join("zeron-headed.log").is_file());
        // Second launch while the first is alive: canonical file untouched,
        // pid-suffixed overflow file instead (the 2026-08-04 clobber).
        let second = open_log_file_in(dir, "headed").expect("second log");
        let pid_path = dir.join(format!("zeron-headed.{}.log", std::process::id()));
        assert!(pid_path.is_file(), "expected pid-suffixed overflow log");
        assert!(
            !dir.join("zeron-headed.log.old").exists(),
            "live canonical log must not be rotated away"
        );
        drop(second);
        // After the owner exits, a fresh launch rotates normally.
        drop(first);
        let third = open_log_file_in(dir, "headed").expect("third log");
        assert!(
            dir.join("zeron-headed.log.old").is_file(),
            "rotation resumes"
        );
        drop(third);
    }
}

/// Delete `zeron-{mode}.{pid}.log` overflow files older than a week — they
/// only exist when a second instance raced a live one for the canonical log.
#[cfg(unix)]
fn sweep_stale_pid_logs(dir: &std::path::Path, mode: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("zeron-{mode}.");
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".log"))
        else {
            continue;
        };
        if !middle.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
