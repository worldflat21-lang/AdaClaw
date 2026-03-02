//! Docker / OCI Container Environment Detection
//!
//! Detects whether the current process is running inside a container.
//! Used to warn when `AutonomyLevel::Full` is used outside a container,
//! which exposes the host filesystem to AI-generated commands.
//!
//! # Detection strategy
//!
//! | Platform | Method |
//! |----------|--------|
//! | Linux    | `/.dockerenv` existence + `/proc/1/cgroup` content parsing |
//! | macOS    | Environment variables (`DOCKER_CONTAINER`, `container`, etc.) |
//! | Windows  | Environment variables only |
//!
//! # Usage
//!
//! ```rust,no_run
//! use adaclaw_security::sandbox::docker::{ContainerEnvironment, WarnLevel};
//! use adaclaw_security::approval::AutonomyLevel;
//!
//! let level = AutonomyLevel::Full;
//! if let Some(warning) = ContainerEnvironment::check_autonomy_safety(&level) {
//!     ContainerEnvironment::print_warning(&warning);
//!     // Optionally abort if security.allow_full_outside_container = false
//! }
//! ```

use crate::approval::AutonomyLevel;

// ── SecurityWarning ───────────────────────────────────────────────────────────

/// The severity of a security warning.
#[derive(Debug, Clone, PartialEq)]
pub enum WarnLevel {
    /// Advisory — operation is allowed but suboptimal.
    Warn,
    /// Critical — potential for serious data loss or system damage.
    Critical,
}

/// A security warning emitted when the runtime configuration is unsafe.
#[derive(Debug, Clone)]
pub struct SecurityWarning {
    /// How serious this warning is.
    pub level: WarnLevel,
    /// Multi-line description of the risk.
    pub message: String,
    /// Actionable mitigation advice.
    pub mitigation: String,
}

// ── ContainerEnvironment ──────────────────────────────────────────────────────

/// Passive container environment detector.
///
/// Only performs read-only checks — never creates or modifies any containers.
pub struct ContainerEnvironment;

impl ContainerEnvironment {
    /// Detect whether the current process is running inside a Docker / OCI container.
    ///
    /// # Platform-specific logic
    ///
    /// **Linux:**
    /// 1. Check `/.dockerenv` (Docker sets this file in every container)
    /// 2. Parse `/proc/1/cgroup` for container runtime signatures
    /// 3. Check environment variables
    ///
    /// **macOS / Windows:**
    /// - Check environment variables only (`DOCKER_CONTAINER`, `container`,
    ///   `KUBERNETES_SERVICE_HOST`)
    pub fn is_running_in_container() -> bool {
        // ── Environment variable checks (cross-platform) ──────────────────────
        if std::env::var("DOCKER_CONTAINER").is_ok()
            || std::env::var("container").is_ok()
            || std::env::var("KUBERNETES_SERVICE_HOST").is_ok()
            || std::env::var("ADACLAW_IN_CONTAINER").is_ok()
        {
            return true;
        }

        // ── Linux-specific file checks ────────────────────────────────────────
        #[cfg(target_os = "linux")]
        {
            // Docker creates this file in every container
            if std::path::Path::new("/.dockerenv").exists() {
                return true;
            }

            // Parse /proc/1/cgroup for container runtime signatures
            if let Ok(content) = std::fs::read_to_string("/proc/1/cgroup") {
                let lower = content.to_lowercase();
                if lower.contains("docker")
                    || lower.contains("lxc")
                    || lower.contains("kubepods")
                    || lower.contains("containerd")
                    || lower.contains("podman")
                {
                    return true;
                }
            }

            // Check /proc/1/environ for container environment markers
            // (some rootless container runtimes don't write /.dockerenv)
            if let Ok(bytes) = std::fs::read("/proc/1/environ") {
                let env = String::from_utf8_lossy(&bytes);
                if env.contains("container=")
                    || env.contains("DOCKER_CONTAINER=")
                    || env.contains("KUBERNETES_SERVICE_HOST=")
                {
                    return true;
                }
            }
        }

        false
    }

    /// Check whether the given `AutonomyLevel` is safe for the current environment.
    ///
    /// Returns `None` if the environment is safe (inside a container, or level ≠ Full).
    /// Returns `Some(SecurityWarning)` if human attention is required.
    ///
    /// This check is purely advisory — the caller decides whether to block or continue.
    pub fn check_autonomy_safety(level: &AutonomyLevel) -> Option<SecurityWarning> {
        // Only Full mode poses a risk — ReadOnly and Supervised have their own guards
        if *level != AutonomyLevel::Full {
            return None;
        }

        // Full mode inside a container is safe by design
        if Self::is_running_in_container() {
            return None;
        }

        // Full mode outside a container on the host — warn loudly
        Some(SecurityWarning {
            level: WarnLevel::Critical,
            message: [
                "⚠️  SECURITY RISK: Full autonomy mode is active OUTSIDE a container!",
                "",
                "  The AI agent can READ, WRITE, and EXECUTE commands on your HOST system.",
                "  If the AI behaves unexpectedly, it may cause irreversible data loss.",
                "  This includes: deleting files, running scripts, modifying system configs.",
            ]
            .join("\n"),
            mitigation: [
                "RECOMMENDED: Run AdaClaw inside Docker for full isolation:",
                "  docker compose up -d",
                "",
                "OR, to explicitly acknowledge this risk, add to config.toml:",
                "  [security]",
                "  allow_full_outside_container = true",
                "",
                "OR pass the flag: adaclaw run --i-know-what-i-am-doing",
            ]
            .join("\n"),
        })
    }

    /// Print a `SecurityWarning` to stderr with ANSI color codes.
    ///
    /// Uses red for `Critical`, yellow for `Warn`.
    /// Falls back to plain text if the terminal doesn't support color.
    pub fn print_warning(warning: &SecurityWarning) {
        let (color, reset) = if Self::supports_ansi() {
            match warning.level {
                WarnLevel::Critical => ("\x1b[1;31m", "\x1b[0m"),
                WarnLevel::Warn => ("\x1b[1;33m", "\x1b[0m"),
            }
        } else {
            ("", "")
        };
        let yellow = if Self::supports_ansi() {
            "\x1b[33m"
        } else {
            ""
        };

        eprintln!();
        eprintln!("{}{}{}", color, warning.message, reset);
        eprintln!();
        eprintln!("{}Mitigation:{}", yellow, reset);
        for line in warning.mitigation.lines() {
            eprintln!("  {}", line);
        }
        eprintln!();
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Returns `true` if the terminal likely supports ANSI escape codes.
    fn supports_ansi() -> bool {
        // Simple heuristic: check NO_COLOR env var and TERM
        if std::env::var("NO_COLOR").is_ok() {
            return false;
        }
        if let Ok(term) = std::env::var("TERM") {
            return term != "dumb";
        }
        // On Windows without a VT100-capable terminal, disable ANSI
        #[cfg(target_os = "windows")]
        {
            std::env::var("WT_SESSION").is_ok() // Windows Terminal
                || std::env::var("ANSICON").is_ok()
        }
        #[cfg(not(target_os = "windows"))]
        {
            true // Most Unix terminals support ANSI
        }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_non_full_mode_always_safe() {
        assert!(ContainerEnvironment::check_autonomy_safety(&AutonomyLevel::ReadOnly).is_none());
        assert!(ContainerEnvironment::check_autonomy_safety(&AutonomyLevel::Supervised).is_none());
    }

    #[test]
    fn test_is_running_in_container_no_panic() {
        // Just check it doesn't panic on any platform
        let _ = ContainerEnvironment::is_running_in_container();
    }

    #[test]
    fn test_full_mode_safety_check_returns_result() {
        // Returns Some or None depending on environment — just ensure no panic
        let _ = ContainerEnvironment::check_autonomy_safety(&AutonomyLevel::Full);
    }

    #[test]
    fn test_warning_message_not_empty() {
        // Construct a synthetic warning (as if Full mode outside container)
        let warning = SecurityWarning {
            level: WarnLevel::Critical,
            message: "Test critical warning".to_string(),
            mitigation: "Run with docker compose".to_string(),
        };
        assert!(!warning.message.is_empty());
        assert!(!warning.mitigation.is_empty());
        // print_warning should not panic
        // (we can't easily suppress stderr in tests, so just call it)
        // ContainerEnvironment::print_warning(&warning);
    }

    #[test]
    fn test_warn_level_equality() {
        assert_eq!(WarnLevel::Critical, WarnLevel::Critical);
        assert_ne!(WarnLevel::Critical, WarnLevel::Warn);
    }
}
