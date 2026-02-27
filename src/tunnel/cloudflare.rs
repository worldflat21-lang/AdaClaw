//! Cloudflare Tunnel integration.
//!
//! Runs `cloudflared tunnel run --token <token>` in the background.
//! The tunnel forwards HTTPS traffic to `localhost:<port>`.
//!
//! ## Prerequisites
//! The `cloudflared` CLI must be installed:
//! - macOS: `brew install cloudflare/cloudflare/cloudflared`
//! - Linux: download from https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/
//! - Windows: download from the same URL

use super::{spawn_process, TunnelHandle};
use std::process::Command;
use tracing::info;

/// Start a Cloudflare Tunnel using a pre-configured tunnel token.
pub fn start(port: u16, token: &str) -> Option<TunnelHandle> {
    info!(port, "Starting Cloudflare Tunnel");

    let mut cmd = Command::new("cloudflared");
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "run",
        "--token",
        token,
        "--url",
        &format!("http://localhost:{}", port),
    ]);

    // Suppress output to avoid polluting logs
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let child = spawn_process("cloudflare", &mut cmd)?;
    Some(TunnelHandle::new("cloudflare", child, None))
}
