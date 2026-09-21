//! Trusted-device pairing and durable peer management. Secrets are read from a
//! private file or stdin, never required on the command line. Running engines
//! own their identity; commands use local IPC rather than racing their stores.

use std::path::Path;

use serde_json::{Value, json};
use kratos_engine::{Engine, EngineConfig, InstanceLock};
use kratos_rpc::{RpcService, methods};

/// Dispatch through the running engine, or take its exclusive lock for a
/// standalone operation. Both paths use the exact same IPC authorization API.
async fn call(config: &EngineConfig, method: &str, params: Value) -> anyhow::Result<Value> {
    std::fs::create_dir_all(&config.data_dir)?;
    match InstanceLock::acquire(&config.data_dir) {
        Ok(_lock) => {
            let auth = Engine::build_auth(config).await?;
            let service = kratos_engine::rpc::AuthRpc::new(auth);
            match service.handle(method, params).await? {
                kratos_rpc::RpcReply::Value(value) => Ok(value),
                kratos_rpc::RpcReply::Stream(_) => anyhow::bail!("expected a unary peer operation"),
            }
        }
        Err(lock_error) => {
            let client = kratos_rpc::connect_ws(&format!("ws://127.0.0.1:{}", config.ipc_port))
                .await
                .map_err(|error| anyhow::anyhow!("{lock_error}; local IPC unavailable: {error}"))?;
            Ok(client.call(method, params).await?)
        }
    }
}

pub async fn initialize(
    config: EngineConfig,
    name: String,
    derp_map: Option<String>,
) -> anyhow::Result<()> {
    call(
        &config,
        methods::PEER_INITIALIZE,
        json!({"name": name, "derpMap": derp_map}),
    )
    .await?;
    println!("Durable sync peer initialized. Start or restart Kratos to use its synced profile.");
    println!(
        "For non-overlapping device availability, keep this peer running on an always-on machine."
    );
    println!(
        "Use `kratos daemon install` to run it as a service, then `kratos peer invite` to pair another device."
    );
    Ok(())
}

pub async fn pair(config: EngineConfig, code_file: Option<&Path>) -> anyhow::Result<()> {
    let code = read_invitation(code_file)?;
    call(&config, methods::PEER_PAIR, json!({"code": code})).await?;
    println!(
        "Device paired. Restart Kratos to open the paired profile; existing local data stays local."
    );
    Ok(())
}

fn read_invitation(file: Option<&Path>) -> anyhow::Result<String> {
    use std::io::{IsTerminal, Read};
    let mut code = String::new();
    if let Some(file) = file {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::symlink_metadata(file)?;
            anyhow::ensure!(
                metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
                "invitation file must be a regular owner-only file (chmod 600)"
            );
        }
        std::fs::File::open(file)?
            .take(64 * 1024 + 1)
            .read_to_string(&mut code)?;
    } else {
        if std::io::stdin().is_terminal() {
            eprintln!(
                "Paste the pairing invitation, then press Enter. It grants trusted-device access."
            );
            std::io::stdin().read_line(&mut code)?;
        } else {
            std::io::stdin()
                .take(64 * 1024 + 1)
                .read_to_string(&mut code)?;
        }
    }
    anyhow::ensure!(
        !code.trim().is_empty() && code.len() <= 64 * 1024,
        "a non-empty pairing invitation of at most 64 KiB is required"
    );
    Ok(code.trim().to_owned())
}

pub async fn invite(config: EngineConfig, output: Option<&Path>) -> anyhow::Result<()> {
    use std::io::Write;
    let result = call(&config, methods::PEER_INVITE, json!({})).await?;
    let code = result
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("peer returned no invitation"))?;
    if let Some(path) = output {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        writeln!(file, "{code}")?;
        file.sync_all()?;
        eprintln!(
            "Saved a one-use invitation. Transfer it privately and remove the file after pairing."
        );
    } else {
        // stdout is the explicitly requested secret export channel, not a log.
        println!("{code}");
        eprintln!(
            "One-use invitation: share privately. Anyone redeeming it becomes a trusted device."
        );
    }
    Ok(())
}

pub async fn devices(config: EngineConfig) -> anyhow::Result<()> {
    let result = call(&config, methods::PEER_DEVICES, json!({})).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub async fn revoke(config: EngineConfig, device_id: String) -> anyhow::Result<()> {
    call(
        &config,
        methods::PEER_REVOKE,
        json!({"deviceId": device_id}),
    )
    .await?;
    println!("Device revoked; its active peer connections are closed.");
    Ok(())
}

pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    call(&config, methods::SIGN_OUT, json!({})).await?;
    println!("Device disconnected. Restart Kratos to return to the unchanged local-only profile.");
    println!("To remove this device's trust remotely, revoke it from the peer owner.");
    Ok(())
}

pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let status = call(&config, methods::PEER_STATUS, json!({})).await?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_file_is_bounded_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("invitation");
        std::fs::write(&file, "  kratos-pair:example\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(read_invitation(Some(&file)).unwrap(), "kratos-pair:example");
        std::fs::write(&file, " ").unwrap();
        assert!(read_invitation(Some(&file)).is_err());
        std::fs::write(&file, vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert!(read_invitation(Some(&file)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn invitation_file_must_not_be_public_or_symlinked() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("invitation");
        std::fs::write(&file, "kratos-pair:example").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_invitation(Some(&file)).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(read_invitation(Some(&link)).is_err());
    }
}
