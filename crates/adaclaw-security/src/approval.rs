//! Approval Manager — AutonomyLevel-based tool execution approval.
//!
//! Three levels of agent autonomy:
//! - `ReadOnly`   — No tool execution permitted (observe only)
//! - `Supervised` — Human must confirm each tool call (CLI only; auto-deny on other channels)
//! - `Full`       — Execute all tools automatically without confirmation
//!
//! # Integration
//!
//! The `ApprovalManager` is consulted before each tool call in the agent engine.
//! When `AutonomyLevel::Supervised` and running in an interactive (CLI) session,
//! the manager prints the tool call details and waits for user confirmation.
//!
//! On non-interactive channels (Telegram, Discord, etc.) Supervised mode
//! auto-denies to prevent blocking the async dispatch loop.

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use tracing::warn;

// ── AutonomyLevel ─────────────────────────────────────────────────────────────

/// The three levels of agent autonomy.
///
/// Configure in `config.toml`:
/// ```toml
/// [security]
/// autonomy_level = "supervised"  # or "readonly" | "full"
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Observe only — no tool execution allowed whatsoever.
    ReadOnly,
    /// Human must confirm each tool call before execution (default).
    ///
    /// On CLI: interactive prompt.
    /// On other channels: auto-denied (don't block async tasks).
    Supervised,
    /// Execute all tools automatically without any human confirmation.
    ///
    /// ⚠️ Recommended only inside a Docker container.
    Full,
}

impl std::str::FromStr for AutonomyLevel {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "readonly" | "read_only" | "read-only" => AutonomyLevel::ReadOnly,
            "full" => AutonomyLevel::Full,
            _ => AutonomyLevel::Supervised,
        })
    }
}

impl AutonomyLevel {
    /// Returns a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            AutonomyLevel::ReadOnly => "ReadOnly — tools disabled, observation only",
            AutonomyLevel::Supervised => "Supervised — human approval required per tool call",
            AutonomyLevel::Full => "Full — all tools execute automatically (no confirmation)",
        }
    }
}

impl std::fmt::Display for AutonomyLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutonomyLevel::ReadOnly => write!(f, "ReadOnly"),
            AutonomyLevel::Supervised => write!(f, "Supervised"),
            AutonomyLevel::Full => write!(f, "Full"),
        }
    }
}

// ── ApprovalDecision ──────────────────────────────────────────────────────────

/// The result of an approval request.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    /// The tool call is approved — proceed with execution.
    Approved,
    /// The tool call is denied — do not execute.
    /// The `String` is the reason (shown to the LLM as tool error output).
    Denied(String),
}

impl ApprovalDecision {
    /// Returns `true` if approved.
    pub fn is_approved(&self) -> bool {
        matches!(self, ApprovalDecision::Approved)
    }

    /// Returns the denial reason, or `None` if approved.
    pub fn denial_reason(&self) -> Option<&str> {
        match self {
            ApprovalDecision::Denied(reason) => Some(reason),
            _ => None,
        }
    }
}

// ── ApprovalManager ───────────────────────────────────────────────────────────

/// Manages tool execution approval based on the configured `AutonomyLevel`.
///
/// Create one per daemon/session and share via `Arc`.
pub struct ApprovalManager {
    /// The configured autonomy level.
    pub level: AutonomyLevel,
    /// Whether interactive stdin/stdout is available for prompts.
    /// Should be `true` only for the CLI channel.
    pub interactive: bool,
}

impl ApprovalManager {
    /// Create a new `ApprovalManager`.
    pub fn new(level: AutonomyLevel, interactive: bool) -> Self {
        Self { level, interactive }
    }

    /// Create from a config string (e.g. `"supervised"`).
    pub fn from_config_str(level_str: &str, interactive: bool) -> Self {
        Self {
            level: level_str.parse().unwrap_or(AutonomyLevel::Supervised),
            interactive,
        }
    }

    // ── Approval logic ────────────────────────────────────────────────────────

    /// Request approval for a tool execution.
    ///
    /// # Behavior per level
    ///
    /// | Level      | Interactive | Result |
    /// |------------|-------------|--------|
    /// | ReadOnly   | any         | Denied  |
    /// | Supervised | true (CLI)  | Prompt user |
    /// | Supervised | false       | Denied (auto) |
    /// | Full       | any         | Approved |
    pub fn approve_tool(&self, tool_name: &str, args_preview: &str) -> ApprovalDecision {
        match &self.level {
            AutonomyLevel::ReadOnly => {
                warn!(tool = %tool_name, "Tool denied: ReadOnly mode");
                ApprovalDecision::Denied(
                    "ReadOnly mode: tool execution is not permitted. \
                     Change `security.autonomy_level` to 'supervised' or 'full' to enable tools."
                        .to_string(),
                )
            }

            AutonomyLevel::Full => ApprovalDecision::Approved,

            AutonomyLevel::Supervised => {
                if self.interactive {
                    self.prompt_interactive(tool_name, args_preview)
                } else {
                    warn!(
                        tool = %tool_name,
                        "Tool auto-denied: Supervised mode on non-interactive channel"
                    );
                    ApprovalDecision::Denied(format!(
                        "Supervised mode: tool '{}' requires human confirmation, \
                         but this channel doesn't support interactive prompts. \
                         Switch to CLI or set autonomy_level = 'full' to allow automatic execution.",
                        tool_name
                    ))
                }
            }
        }
    }

    // ── Interactive prompt ────────────────────────────────────────────────────

    /// Display a tool call prompt and wait for user input (y/N).
    fn prompt_interactive(&self, tool_name: &str, args_preview: &str) -> ApprovalDecision {
        println!();
        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║  🔧  TOOL CALL REQUEST                                       ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!("  Tool : {}", tool_name);

        // Truncate long arg previews for readability
        let args_display = if args_preview.len() > 200 {
            format!("{}… (truncated)", &args_preview[..200])
        } else {
            args_preview.to_string()
        };
        println!("  Args : {}", args_display);
        println!();
        print!("  Allow execution? [y/N]: ");
        io::stdout().flush().unwrap_or(());

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(_) => {
                let trimmed = input.trim().to_lowercase();
                if trimmed == "y" || trimmed == "yes" {
                    println!("  ✅ Approved.\n");
                    ApprovalDecision::Approved
                } else {
                    println!("  ❌ Denied.\n");
                    ApprovalDecision::Denied(format!(
                        "Tool '{}' was denied by the user.",
                        tool_name
                    ))
                }
            }
            Err(e) => {
                warn!("Failed to read approval input: {}", e);
                ApprovalDecision::Denied(
                    "Failed to read approval input from stdin.".to_string(),
                )
            }
        }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readonly_always_denies() {
        let mgr = ApprovalManager::new(AutonomyLevel::ReadOnly, false);
        let d = mgr.approve_tool("shell", "echo hello");
        assert!(!d.is_approved());
        assert!(d.denial_reason().is_some());
    }

    #[test]
    fn test_readonly_interactive_still_denies() {
        let mgr = ApprovalManager::new(AutonomyLevel::ReadOnly, true);
        let d = mgr.approve_tool("file_write", "test.txt");
        assert!(!d.is_approved());
    }

    #[test]
    fn test_full_always_approves() {
        let mgr = ApprovalManager::new(AutonomyLevel::Full, false);
        let d = mgr.approve_tool("shell", "rm -rf /tmp/test");
        assert!(d.is_approved());
    }

    #[test]
    fn test_supervised_non_interactive_denies() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let d = mgr.approve_tool("file_write", "data.txt");
        assert!(!d.is_approved());
        assert!(d.denial_reason().unwrap().contains("Supervised mode"));
    }

    #[test]
    fn test_autonomy_level_from_str() {
        assert_eq!("readonly".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::ReadOnly);
        assert_eq!("ReadOnly".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::ReadOnly);
        assert_eq!("read_only".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::ReadOnly);
        assert_eq!("full".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::Full);
        assert_eq!("FULL".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::Full);
        assert_eq!("supervised".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::Supervised);
        assert_eq!("unknown".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::Supervised);
        assert_eq!("".parse::<AutonomyLevel>().unwrap(), AutonomyLevel::Supervised);
    }

    #[test]
    fn test_from_config_str() {
        let mgr = ApprovalManager::from_config_str("full", false);
        assert_eq!(mgr.level, AutonomyLevel::Full);

        let mgr = ApprovalManager::from_config_str("readonly", true);
        assert_eq!(mgr.level, AutonomyLevel::ReadOnly);
    }

    #[test]
    fn test_approval_decision_helpers() {
        let approved = ApprovalDecision::Approved;
        assert!(approved.is_approved());
        assert!(approved.denial_reason().is_none());

        let denied = ApprovalDecision::Denied("reason".to_string());
        assert!(!denied.is_approved());
        assert_eq!(denied.denial_reason(), Some("reason"));
    }

    #[test]
    fn test_autonomy_level_display() {
        assert_eq!(AutonomyLevel::ReadOnly.to_string(), "ReadOnly");
        assert_eq!(AutonomyLevel::Supervised.to_string(), "Supervised");
        assert_eq!(AutonomyLevel::Full.to_string(), "Full");
    }
}
