//! Structured Audit Log — writes JSONL events to a rotating file.
//!
//! Each event is serialized as a single JSON line, compatible with SIEM
//! systems (Splunk, Elastic, Loki, Datadog, etc.).
//!
//! # Usage
//!
//! ```rust,ignore
//! use adaclaw_security::audit::AuditLogger;
//! let logger = AuditLogger::new("./logs/audit.jsonl").unwrap();
//! logger.log_tool("shell", true, r#"{"command": "ls"}"#, Some("assistant"));
//! logger.log_unauthorized("user123", "telegram", "not in allowlist");
//! ```
//!
//! # JSONL format
//!
//! ```json
//! {"timestamp":"2026-02-27T10:30:00Z","kind":"tool_executed","agent_id":"assistant","tool":"shell","success":true,"args_preview":"{\"command\":\"ls\"}"}
//! {"timestamp":"2026-02-27T10:30:01Z","kind":"unauthorized_access","sender_id":"bad_actor","channel":"telegram","reason":"not in allowlist"}
//! ```

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::{debug, warn};

// ── AuditKind ─────────────────────────────────────────────────────────────────

/// The type of event being logged, with its associated data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditKind {
    /// A tool was executed by an agent.
    ToolExecuted {
        tool: String,
        success: bool,
        /// First 200 chars of the serialized arguments.
        args_preview: String,
    },

    /// A file was accessed.
    FileAccessed {
        path: String,
        /// "read" | "write" | "delete" | "list"
        operation: String,
    },

    /// An inbound message was rejected because the sender is not in the allowlist.
    UnauthorizedAccess {
        sender_id: String,
        channel: String,
        reason: String,
    },

    /// Emergency stop was engaged.
    EstopEngaged {
        level: String,
        reason: Option<String>,
    },

    /// Emergency stop was cleared.
    EstopCleared,

    /// A rate limit was exceeded.
    RateLimitExceeded {
        sender_id: String,
        channel: String,
        /// "per_user" | "per_channel" | "daily_cost" | "max_actions"
        limit_type: String,
    },

    /// An inbound message was received (before agent dispatch).
    MessageReceived {
        channel: String,
        sender_id: String,
        /// First 100 chars of the message content.
        content_preview: String,
    },

    /// An agent started processing a message.
    AgentStarted {
        agent_id: String,
        model: String,
    },

    /// An agent encountered an unrecoverable error.
    AgentError {
        agent_id: String,
        error: String,
    },

    /// An OTP verification attempt was made.
    OtpVerified {
        success: bool,
    },

    /// A secret was accessed from the encrypted store.
    SecretAccessed {
        key: String,
    },

    /// A sub-agent task was delegated.
    SubagentDelegated {
        parent_agent: String,
        target_agent: String,
        task_preview: String,
    },

    /// Daemon startup event.
    DaemonStarted {
        version: String,
        autonomy_level: String,
    },

    /// Daemon shutdown event.
    DaemonStopped,
}

// ── AuditEvent ────────────────────────────────────────────────────────────────

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// RFC 3339 UTC timestamp.
    pub timestamp: String,

    /// Agent ID (when the event is associated with a specific agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// Channel (when the event is associated with a specific channel).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,

    /// Sender/user ID (when applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,

    /// The event kind and its inline data.
    #[serde(flatten)]
    pub kind: AuditKind,
}

impl AuditEvent {
    /// Create a new event with the current UTC timestamp.
    pub fn new(kind: AuditKind) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            agent_id: None,
            channel: None,
            sender_id: None,
            kind,
        }
    }

    /// Set the agent ID context.
    pub fn with_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Set the channel context.
    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    /// Set the sender/user ID context.
    pub fn with_sender(mut self, sender_id: impl Into<String>) -> Self {
        self.sender_id = Some(sender_id.into());
        self
    }
}

// ── AuditLogger ───────────────────────────────────────────────────────────────

/// Writes audit events to a JSONL file.
///
/// The file is opened in append mode so logs survive restarts.
/// A `Mutex` serializes concurrent writes.
///
/// # Failure handling
///
/// All write failures are logged to `tracing::warn` but **never panic** —
/// audit log failures must not interrupt the agent loop.
pub struct AuditLogger {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl AuditLogger {
    /// Open (or create) the audit log at `path`. Parent directories are created.
    pub fn new(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Log an arbitrary audit event. Non-blocking, never panics.
    pub fn log(&self, event: AuditEvent) {
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "Failed to serialize audit event");
                return;
            }
        };

        debug!(json = %json, "Audit event");

        let mut file = self.file.lock().unwrap();
        if let Err(e) = writeln!(file, "{}", json) {
            warn!(error = %e, path = ?self.path, "Failed to write audit event to log");
        }
    }

    // ── Convenience methods ───────────────────────────────────────────────────

    /// Log a tool execution result.
    pub fn log_tool(&self, tool: &str, success: bool, args_preview: &str, agent_id: Option<&str>) {
        let event = AuditEvent::new(AuditKind::ToolExecuted {
            tool: tool.to_string(),
            success,
            args_preview: args_preview.chars().take(200).collect(),
        });
        let event = match agent_id {
            Some(id) => event.with_agent(id),
            None => event,
        };
        self.log(event);
    }

    /// Log an unauthorized access rejection.
    pub fn log_unauthorized(&self, sender_id: &str, channel: &str, reason: &str) {
        self.log(
            AuditEvent::new(AuditKind::UnauthorizedAccess {
                sender_id: sender_id.to_string(),
                channel: channel.to_string(),
                reason: reason.to_string(),
            })
            .with_sender(sender_id)
            .with_channel(channel),
        );
    }

    /// Log an estop engagement.
    pub fn log_estop_engaged(&self, level: &str, reason: Option<&str>) {
        self.log(AuditEvent::new(AuditKind::EstopEngaged {
            level: level.to_string(),
            reason: reason.map(str::to_string),
        }));
    }

    /// Log an estop clear.
    pub fn log_estop_cleared(&self) {
        self.log(AuditEvent::new(AuditKind::EstopCleared));
    }

    /// Log a rate limit exceeded event.
    pub fn log_rate_limit(
        &self,
        sender_id: &str,
        channel: &str,
        limit_type: &str,
    ) {
        self.log(
            AuditEvent::new(AuditKind::RateLimitExceeded {
                sender_id: sender_id.to_string(),
                channel: channel.to_string(),
                limit_type: limit_type.to_string(),
            })
            .with_sender(sender_id)
            .with_channel(channel),
        );
    }

    /// Log a message received event.
    pub fn log_message(&self, channel: &str, sender_id: &str, content_preview: &str) {
        self.log(
            AuditEvent::new(AuditKind::MessageReceived {
                channel: channel.to_string(),
                sender_id: sender_id.to_string(),
                content_preview: content_preview.chars().take(100).collect(),
            })
            .with_channel(channel)
            .with_sender(sender_id),
        );
    }

    /// Log daemon startup.
    pub fn log_started(&self, version: &str, autonomy_level: &str) {
        self.log(AuditEvent::new(AuditKind::DaemonStarted {
            version: version.to_string(),
            autonomy_level: autonomy_level.to_string(),
        }));
    }

    /// Log daemon shutdown.
    pub fn log_stopped(&self) {
        self.log(AuditEvent::new(AuditKind::DaemonStopped));
    }

    /// Return the path of the audit log file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read all stored audit events (for `adaclaw status` / Web UI).
    pub fn read_all(&self) -> Vec<AuditEvent> {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (AuditLogger, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        (AuditLogger::new(path).unwrap(), dir)
    }

    #[test]
    fn test_log_tool_no_panic() {
        let (logger, _dir) = setup();
        logger.log_tool("shell", true, r#"{"command": "ls"}"#, Some("assistant"));
    }

    #[test]
    fn test_log_creates_file_and_is_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        {
            let logger = AuditLogger::new(&path).unwrap();
            logger.log_unauthorized("user1", "telegram", "not in allowlist");
            logger.log_tool("file_write", false, r#"{"path":"/etc/passwd"}"#, None);
        }
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have 2 JSONL lines");

        // Each line must be valid JSON
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("timestamp").is_some(), "must have timestamp");
            assert!(v.get("kind").is_some(), "must have kind");
        }
    }

    #[test]
    fn test_log_estop_events() {
        let (logger, dir) = setup();
        logger.log_estop_engaged("KillAll", Some("test reason"));
        logger.log_estop_cleared();

        let path = dir.path().join("audit.jsonl");
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("estop_engaged"));
        assert!(content.contains("test reason"));
        assert!(content.contains("estop_cleared"));
    }

    #[test]
    fn test_event_builder_chain() {
        let event = AuditEvent::new(AuditKind::AgentStarted {
            agent_id: "coder".to_string(),
            model: "gpt-4o".to_string(),
        })
        .with_agent("coder")
        .with_channel("telegram")
        .with_sender("user42");

        assert_eq!(event.agent_id.as_deref(), Some("coder"));
        assert_eq!(event.channel.as_deref(), Some("telegram"));
        assert_eq!(event.sender_id.as_deref(), Some("user42"));
    }

    #[test]
    fn test_read_all_returns_logged_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        {
            let logger = AuditLogger::new(&path).unwrap();
            logger.log_started("0.1.0", "supervised");
            logger.log_stopped();
        }
        let logger2 = AuditLogger::new(&path).unwrap();
        let events = logger2.read_all();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_log_rate_limit() {
        let (logger, dir) = setup();
        logger.log_rate_limit("user1", "telegram", "per_user");
        let path = dir.path().join("audit.jsonl");
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("rate_limit_exceeded"));
        assert!(content.contains("per_user"));
    }

    #[test]
    fn test_log_message() {
        let (logger, dir) = setup();
        logger.log_message("cli", "user1", "help me with this code");
        let path = dir.path().join("audit.jsonl");
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("message_received"));
        assert!(content.contains("cli"));
    }

    #[test]
    fn test_parent_dirs_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("audit.jsonl");
        let logger = AuditLogger::new(&path).unwrap();
        logger.log_stopped();
        assert!(path.exists());
    }
}
