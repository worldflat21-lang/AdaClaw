//! Tailscale tunnel integration.
//!
//! In **Funnel mode** (public internet): runs `tailscale funnel <port>`
//! In **Serve mode** (tailnet only):    runs `tailscale serve <port>`
//!
//! ## Prerequisites
//! Tailscale must be installed and authenticated (`tailscale up`) before use.
//! Funnel requires Tailscale 1.36+ and the Funnel feature enabled on your tailnet.

use super::{TunnelHandle, spawn_process};
use std::process::Command;
use tracing::info;

/// Start a Tailscale Serve or Funnel tunnel.
///
/// - `funnel = true`  → `tailscale funnel <port>` (public internet access)
/// - `funnel = false` → `tailscale serve <port>`  (tailnet only)
pub fn start(port: u16, funnel: bool) -> Option<TunnelHandle> {
    let mode = if funnel { "funnel" } else { "serve" };
    info!(port, mode, "Starting Tailscale tunnel");

    let mut cmd = Command::new("tailscale");
    cmd.args([mode, &port.to_string()]);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let child = spawn_process("tailscale", &mut cmd)?;
    Some(TunnelHandle::new("tailscale", child, None))
}
