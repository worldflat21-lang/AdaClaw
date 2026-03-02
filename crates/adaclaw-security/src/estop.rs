//! Estop — Emergency Stop Controller
//!
//! Provides 4 graduated emergency stop levels:
//! - `KillAll`     — Stops all agent processing immediately (hard stop)
//! - `NetworkKill` — Blocks all outbound network calls (API, HTTP)
//! - `DomainBlock` — Blocks specific domains from being accessed
//! - `ToolFreeze`  — Freezes tool execution; LLM can still think and reply
//!
//! State is persisted to disk as JSON so it survives process restarts.
//! A broadcast channel notifies all subscribers when estop is engaged.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

// ── EstopLevel ────────────────────────────────────────────────────────────────

/// The 4 levels of emergency stop, in increasing severity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum EstopLevel {
    /// Freeze tool execution only. LLM can still respond without tools.
    ToolFreeze,
    /// Block specific domains (list of domain suffixes to block).
    DomainBlock(Vec<String>),
    /// Block ALL outbound network calls — includes all HTTP/API requests.
    NetworkKill,
    /// Stop all agent processing immediately. Hard stop.
    KillAll,
}

impl std::fmt::Display for EstopLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EstopLevel::KillAll => write!(f, "KillAll"),
            EstopLevel::NetworkKill => write!(f, "NetworkKill"),
            EstopLevel::DomainBlock(domains) => write!(f, "DomainBlock({})", domains.join(",")),
            EstopLevel::ToolFreeze => write!(f, "ToolFreeze"),
        }
    }
}

// ── EstopState ────────────────────────────────────────────────────────────────

/// Persistent estop state (written to disk so it survives restarts).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EstopState {
    /// Whether an estop is currently active.
    pub active: bool,
    /// The current estop level (`None` when inactive).
    pub level: Option<EstopLevel>,
    /// RFC 3339 timestamp when the estop was triggered.
    pub triggered_at: Option<String>,
    /// Human-readable reason for the estop.
    pub reason: Option<String>,
    /// Whether OTP verification is required to clear this estop.
    pub require_otp: bool,
}

// ── EstopController ───────────────────────────────────────────────────────────

/// Shared estop controller. Cheap to clone (all state behind `Arc`).
///
/// # Usage
///
/// ```rust,ignore
/// use adaclaw_security::estop::{EstopController, EstopLevel};
/// let ctrl = EstopController::new("./.adaclaw/estop.json");
/// ctrl.engage(EstopLevel::ToolFreeze, Some("suspicious behavior".into()), false).unwrap();
/// assert!(ctrl.is_tool_frozen());
/// ctrl.clear(false).unwrap();
/// ```
#[derive(Clone)]
pub struct EstopController {
    state: Arc<RwLock<EstopState>>,
    state_path: PathBuf,
    /// Broadcast channel: subscribers receive the level when estop is engaged.
    pub notify_tx: broadcast::Sender<EstopLevel>,
}

impl EstopController {
    /// Create a new controller. Loads persisted state from `state_path` if it exists.
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        let (tx, _) = broadcast::channel(32);
        let state_path = state_path.into();

        let state = if state_path.exists() {
            std::fs::read_to_string(&state_path)
                .ok()
                .and_then(|s| serde_json::from_str::<EstopState>(&s).ok())
                .unwrap_or_default()
        } else {
            EstopState::default()
        };

        if state.active {
            warn!(
                level = ?state.level,
                reason = ?state.reason,
                triggered_at = ?state.triggered_at,
                "⚠️  ESTOP IS ACTIVE from previous session! Agents will not run."
            );
        }

        Self {
            state: Arc::new(RwLock::new(state)),
            state_path,
            notify_tx: tx,
        }
    }

    // ── State queries ─────────────────────────────────────────────────────────

    /// Returns `true` if any estop level is currently active.
    pub fn is_active(&self) -> bool {
        self.state.read().unwrap().active
    }

    /// Returns `true` if `KillAll` is active (all agents stopped).
    pub fn is_killed(&self) -> bool {
        matches!(self.state.read().unwrap().level, Some(EstopLevel::KillAll))
    }

    /// Returns `true` if network is killed (`NetworkKill` or `KillAll`).
    pub fn is_network_killed(&self) -> bool {
        let s = self.state.read().unwrap();
        matches!(
            s.level,
            Some(EstopLevel::NetworkKill) | Some(EstopLevel::KillAll)
        )
    }

    /// Returns `true` if tool execution is frozen (`ToolFreeze` or higher).
    pub fn is_tool_frozen(&self) -> bool {
        let s = self.state.read().unwrap();
        matches!(
            s.level,
            Some(EstopLevel::ToolFreeze)
                | Some(EstopLevel::NetworkKill)
                | Some(EstopLevel::KillAll)
        )
    }

    /// Returns `true` if the given domain is blocked.
    ///
    /// Checks against suffix matching: blocking "evil.com" also blocks "api.evil.com".
    pub fn is_domain_blocked(&self, domain: &str) -> bool {
        let s = self.state.read().unwrap();
        match &s.level {
            Some(EstopLevel::DomainBlock(domains)) => domains
                .iter()
                .any(|d| domain == d.as_str() || domain.ends_with(&format!(".{}", d))),
            Some(EstopLevel::NetworkKill) | Some(EstopLevel::KillAll) => true,
            _ => false,
        }
    }

    /// Return a snapshot of the current estop state.
    pub fn state(&self) -> EstopState {
        self.state.read().unwrap().clone()
    }

    // ── State mutations ───────────────────────────────────────────────────────

    /// Engage the estop at the given level.
    ///
    /// - Overwrites any existing (lower) estop state.
    /// - Persists state to disk.
    /// - Broadcasts the level to all subscribers.
    /// - If `require_otp` is `true`, OTP must be verified to call `clear()`.
    pub fn engage(
        &self,
        level: EstopLevel,
        reason: Option<String>,
        require_otp: bool,
    ) -> Result<()> {
        info!(
            level = %level,
            reason = ?reason,
            require_otp,
            "🚨 EMERGENCY STOP ENGAGED"
        );
        eprintln!(
            "\x1b[1;31m🚨 EMERGENCY STOP: {} — {}\x1b[0m",
            level,
            reason.as_deref().unwrap_or("no reason given")
        );

        let new_state = EstopState {
            active: true,
            level: Some(level.clone()),
            triggered_at: Some(Utc::now().to_rfc3339()),
            reason,
            require_otp,
        };

        *self.state.write().unwrap() = new_state;
        self.persist()?;

        // Notify all subscribers (ignore "no receivers" error)
        let _ = self.notify_tx.send(level);

        Ok(())
    }

    /// Clear the estop and resume normal operation.
    ///
    /// If `require_otp` was set when engaging, `otp_verified` must be `true`.
    /// The caller is responsible for verifying the OTP before passing `true`.
    pub fn clear(&self, otp_verified: bool) -> Result<()> {
        let requires_otp = self.state.read().unwrap().require_otp;

        if requires_otp && !otp_verified {
            anyhow::bail!(
                "OTP verification required to clear estop. \
                 Run `adaclaw stop --clear --otp <code>` with the correct TOTP code."
            );
        }

        info!("✅ Emergency stop cleared — normal operation resumed");
        eprintln!("\x1b[1;32m✅ Emergency stop cleared.\x1b[0m");

        *self.state.write().unwrap() = EstopState::default();
        self.persist()?;

        Ok(())
    }

    /// Subscribe to estop notifications.
    ///
    /// The returned receiver will yield the `EstopLevel` whenever `engage()` is called.
    pub fn subscribe(&self) -> broadcast::Receiver<EstopLevel> {
        self.notify_tx.subscribe()
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let s = self.state.read().unwrap();
        let json = serde_json::to_string_pretty(&*s)?;
        std::fs::write(&self.state_path, json).map_err(|e| {
            error!(
                "Failed to persist estop state to {:?}: {}",
                self.state_path, e
            );
            e
        })?;
        Ok(())
    }
}

// ── Global instance (optional convenience) ────────────────────────────────────

use std::sync::OnceLock;

static GLOBAL_ESTOP: OnceLock<EstopController> = OnceLock::new();

/// Initialize the global estop controller. Call once at startup.
pub fn init_global(state_path: impl Into<PathBuf>) -> &'static EstopController {
    GLOBAL_ESTOP.get_or_init(|| EstopController::new(state_path))
}

/// Get the global estop controller. Returns `None` if not initialized.
pub fn global() -> Option<&'static EstopController> {
    GLOBAL_ESTOP.get()
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_ctrl() -> (EstopController, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("estop.json");
        (EstopController::new(path), dir)
    }

    #[test]
    fn test_default_inactive() {
        let (ctrl, _dir) = tmp_ctrl();
        assert!(!ctrl.is_active());
        assert!(!ctrl.is_killed());
        assert!(!ctrl.is_tool_frozen());
        assert!(!ctrl.is_network_killed());
    }

    #[test]
    fn test_engage_kill_all() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(EstopLevel::KillAll, Some("test".to_string()), false)
            .unwrap();
        assert!(ctrl.is_active());
        assert!(ctrl.is_killed());
        assert!(ctrl.is_tool_frozen());
        assert!(ctrl.is_network_killed());
    }

    #[test]
    fn test_engage_tool_freeze() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(EstopLevel::ToolFreeze, None, false).unwrap();
        assert!(ctrl.is_active());
        assert!(ctrl.is_tool_frozen());
        assert!(!ctrl.is_killed());
        assert!(!ctrl.is_network_killed());
    }

    #[test]
    fn test_engage_network_kill() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(EstopLevel::NetworkKill, None, false).unwrap();
        assert!(ctrl.is_network_killed());
        assert!(ctrl.is_tool_frozen()); // NetworkKill implies ToolFreeze
        assert!(!ctrl.is_killed());
    }

    #[test]
    fn test_engage_domain_block() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(
            EstopLevel::DomainBlock(vec!["evil.com".to_string(), "bad.org".to_string()]),
            None,
            false,
        )
        .unwrap();
        assert!(ctrl.is_active());
        assert!(ctrl.is_domain_blocked("evil.com"));
        assert!(ctrl.is_domain_blocked("api.evil.com")); // subdomain check
        assert!(ctrl.is_domain_blocked("bad.org"));
        assert!(!ctrl.is_domain_blocked("good.com"));
        assert!(!ctrl.is_tool_frozen()); // DomainBlock alone doesn't freeze tools
    }

    #[test]
    fn test_clear_without_otp() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(EstopLevel::ToolFreeze, None, false).unwrap();
        assert!(ctrl.is_active());
        ctrl.clear(false).unwrap();
        assert!(!ctrl.is_active());
    }

    #[test]
    fn test_clear_requires_otp() {
        let (ctrl, _dir) = tmp_ctrl();
        ctrl.engage(EstopLevel::KillAll, Some("critical event".into()), true)
            .unwrap();
        assert!(ctrl.clear(false).is_err(), "should require OTP");
        ctrl.clear(true).unwrap();
        assert!(!ctrl.is_active());
    }

    #[test]
    fn test_state_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("estop.json");

        {
            let ctrl = EstopController::new(&path);
            ctrl.engage(EstopLevel::NetworkKill, Some("test persist".into()), false)
                .unwrap();
        }

        // New controller instance loading the same file
        let ctrl2 = EstopController::new(&path);
        assert!(ctrl2.is_active());
        assert!(ctrl2.is_network_killed());
        let state = ctrl2.state();
        assert_eq!(state.reason.as_deref(), Some("test persist"));
    }

    #[test]
    fn test_broadcast_notification() {
        let (ctrl, _dir) = tmp_ctrl();
        let mut rx = ctrl.subscribe();
        ctrl.engage(EstopLevel::ToolFreeze, None, false).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received, EstopLevel::ToolFreeze);
    }
}
