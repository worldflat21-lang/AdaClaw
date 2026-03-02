//! Tunnel integrations — expose the local gateway to the internet.
//!
//! Supported providers:
//! - `cloudflare` — Cloudflare Tunnel (Zero Trust, free tier)
//! - `tailscale`  — Tailscale Funnel (private tailnet or public)
//! - `ngrok`      — ngrok (instant public URLs)
//! - `none`       — disabled (local only, default)

pub mod cloudflare;
pub mod ngrok;
pub mod tailscale;

use std::process::{Child, Command};
use tracing::{info, warn};

/// A running tunnel process handle.
pub struct TunnelHandle {
    pub provider: String,
    pub public_url: Option<String>,
    child: Option<Child>,
}

impl TunnelHandle {
    fn new(provider: &str, child: Child, public_url: Option<String>) -> Self {
        Self {
            provider: provider.to_string(),
            public_url,
            child: Some(child),
        }
    }

    /// Kill the tunnel process.
    pub fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
            info!("Tunnel '{}' stopped", self.provider);
        }
        self.child = None;
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the configured tunnel. Returns `None` if `provider = "none"`.
pub fn start_tunnel(
    provider: &str,
    port: u16,
    cloudflare_token: Option<&str>,
    ngrok_token: Option<&str>,
    ngrok_domain: Option<&str>,
    tailscale_funnel: bool,
) -> Option<TunnelHandle> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "cloudflare" | "cf" => cloudflare::start(port, cloudflare_token?),
        "ngrok" => ngrok::start(port, ngrok_token, ngrok_domain),
        "tailscale" | "ts" => tailscale::start(port, tailscale_funnel),
        "none" | "" => None,
        other => {
            warn!("Unknown tunnel provider '{}', tunnel disabled", other);
            None
        }
    }
}

/// Spawn a child process, logging any spawn failure.
pub(crate) fn spawn_process(name: &str, cmd: &mut Command) -> Option<Child> {
    match cmd.spawn() {
        Ok(child) => {
            info!("Tunnel '{}' process started (pid={})", name, child.id());
            Some(child)
        }
        Err(e) => {
            warn!("Failed to spawn tunnel '{}': {}", name, e);
            None
        }
    }
}
