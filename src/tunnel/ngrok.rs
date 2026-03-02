//! ngrok tunnel integration.
//!
//! Runs `ngrok http <port> [--domain <domain>]` in the background.
//!
//! ## Prerequisites
//! The `ngrok` CLI must be installed and authenticated:
//! - Install: https://ngrok.com/download
//! - Authenticate: `ngrok config add-authtoken <token>`
//!
//! ## Programmatic auth token
//! If `auth_token` is provided, this module writes a minimal ngrok config
//! to a temp file and passes it via `--config`, avoiding the need for the
//! user to pre-authenticate the CLI manually.

use super::{TunnelHandle, spawn_process};
use std::process::Command;
use tracing::info;

/// Start an ngrok HTTP tunnel on `port`.
///
/// - `auth_token` — optional ngrok auth token (writes temp config if provided)
/// - `domain`     — optional custom domain (requires ngrok paid plan)
pub fn start(port: u16, auth_token: Option<&str>, domain: Option<&str>) -> Option<TunnelHandle> {
    info!(port, domain = ?domain, "Starting ngrok tunnel");

    let mut args = vec!["http".to_string(), port.to_string()];

    if let Some(d) = domain {
        args.push("--domain".to_string());
        args.push(d.to_string());
    }

    // If auth token is provided, write a minimal temp config
    let _temp_config = if let Some(token) = auth_token {
        let config_content = format!("version: \"2\"\nauthtoken: {token}\n");
        let temp_path = std::env::temp_dir().join("adaclaw-ngrok.yml");
        if std::fs::write(&temp_path, &config_content).is_ok() {
            args.push("--config".to_string());
            args.push(temp_path.to_string_lossy().to_string());
            Some(temp_path)
        } else {
            None
        }
    } else {
        None
    };

    let mut cmd = Command::new("ngrok");
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let child = spawn_process("ngrok", &mut cmd)?;
    Some(TunnelHandle::new("ngrok", child, None))
}
