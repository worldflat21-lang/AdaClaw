//! Approval Manager — AutonomyLevel-based tool execution approval.
//!
//! Three levels of agent autonomy:
//! - `ReadOnly`   — No tool execution permitted (observe only)
//! - `Supervised` — Human must confirm each tool call
//! - `Full`       — Execute all tools automatically without confirmation
//!
//! # Phase 11 Round 5 additions
//!
//! - **`auto_approve` list** — tools that skip confirmation in Supervised mode
//! - **`always_ask` list** — tools that always require confirmation (overrides session allowlist)
//! - **Session allowlist** — CLI "Always" response adds tool for the remainder of the session
//! - **Non-CLI session allowlist** — explicit `grant_non_cli_session()` / `revoke_non_cli_session()`
//!   for non-interactive channels (granted via `/approve-allow` commands after Telegram button press)
//! - **One-time "allow all" token** — `grant_non_cli_allow_all_once()` lets the current turn
//!   execute without prompts (consumed immediately by `consume_non_cli_allow_all_once()`)
//! - **Pending approval requests** — created for non-CLI Supervised mode; expire after
//!   `approval_timeout_minutes` (default 30); same-sender/channel/reply_target required for confirm/reject
//! - **Audit log** — all approval decisions are recorded with timestamp + channel
//! - **CLI prompt Y/N/A** — "A" (Always) adds tool to session allowlist for the remainder of the session

use chrono::{DateTime, Duration, Utc};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Mutex;
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
    /// On CLI: interactive Y/N/A prompt.
    /// On other channels: pending request created; auto-denied until explicitly approved.
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

// ── ApprovalResponse (internal CLI prompt result) ─────────────────────────────

/// The user's response to an interactive CLI approval prompt.
///
/// This is distinct from `ApprovalDecision` — it represents the raw user
/// input before any session allowlist update is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this single tool call.
    Yes,
    /// Deny this tool call.
    No,
    /// Execute and add tool to the session-scoped allowlist (skip future prompts for this tool).
    Always,
}

// ── ApprovalLogEntry ──────────────────────────────────────────────────────────

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub approved: bool,
    pub channel: String,
}

// ── PendingApprovalRequest ────────────────────────────────────────────────────

/// A pending approval request for a non-CLI channel.
///
/// Created by `ApprovalManager::create_pending_request()` and confirmed or rejected
/// when the user presses the Approve/Deny button on Telegram (or sends a
/// `/approve-allow` / `/approve-deny` command).
///
/// Requests expire after `approval_timeout_minutes` (default 30).
/// The requester must be the same sender on the same channel to confirm/reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalRequest {
    /// Unique request ID: `"apr-{10 hex chars}"`.
    pub request_id: String,
    /// Tool name being requested.
    pub tool_name: String,
    /// Short preview of tool arguments.
    pub args_preview: String,
    /// Sender ID of the user who triggered the tool call.
    pub requested_by: String,
    /// Channel name (e.g. `"telegram"`).
    pub requested_channel: String,
    /// Session/reply target (e.g. Telegram chat_id).
    pub requested_reply_target: String,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 expiry timestamp.
    pub expires_at: String,
}

/// Errors from `confirm_pending_request` / `reject_pending_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingApprovalError {
    /// No request with this ID exists (already consumed, never existed, or expired + pruned).
    NotFound,
    /// The request was found but has already expired.
    Expired,
    /// The confirming sender/channel/reply_target does not match the original request.
    RequesterMismatch,
}

impl std::fmt::Display for PendingApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingApprovalError::NotFound => write!(f, "Approval request not found"),
            PendingApprovalError::Expired => write!(f, "Approval request has expired"),
            PendingApprovalError::RequesterMismatch => {
                write!(f, "Approval request sender/channel mismatch")
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn generate_request_id() -> String {
    let mut bytes = [0u8; 5];
    OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("apr-{}", hex)
}

fn is_expired(req: &PendingApprovalRequest) -> bool {
    DateTime::parse_from_rfc3339(&req.expires_at)
        .map(|dt| dt.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(true)
}

fn prune_expired(pending: &mut HashMap<String, PendingApprovalRequest>) {
    pending.retain(|_, req| !is_expired(req));
}

fn summarize_args(args: &str, max: usize) -> String {
    let char_count = args.chars().count();
    if char_count <= max {
        args.to_string()
    } else {
        let truncated: String = args.chars().take(max).collect();
        format!("{}…", truncated)
    }
}

// ── ApprovalManager ───────────────────────────────────────────────────────────

/// Manages tool execution approval based on the configured `AutonomyLevel`.
///
/// Create one per daemon/session and share via `Arc`.
///
/// # Example
///
/// ```no_run
/// use adaclaw_security::approval::{ApprovalManager, AutonomyLevel};
///
/// let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
///     .with_auto_approve(vec!["file_read".into(), "memory_recall".into()])
///     .with_always_ask(vec!["shell".into()]);
/// ```
pub struct ApprovalManager {
    /// The configured autonomy level.
    pub level: AutonomyLevel,
    /// Whether interactive stdin/stdout is available for prompts.
    /// Should be `true` only for the CLI channel.
    pub interactive: bool,
    /// Tools that never need approval in Supervised mode (config + runtime).
    auto_approve: Mutex<HashSet<String>>,
    /// Tools that always need approval, overriding session allowlists (config + runtime).
    always_ask: Mutex<HashSet<String>>,
    /// Session-scoped allowlist built from CLI "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Non-CLI session allowlist — granted explicitly via `grant_non_cli_session()`.
    /// Populated when the user presses "Approve" on a Telegram inline keyboard.
    non_cli_session_allowlist: Mutex<HashSet<String>>,
    /// One-time "allow all tools for one turn" bypass tokens.
    non_cli_allow_all_once: Mutex<u32>,
    /// Pending non-CLI approval requests awaiting human confirmation.
    pending_requests: Mutex<HashMap<String, PendingApprovalRequest>>,
    /// Approval request timeout in minutes (default 30).
    approval_timeout_minutes: i64,
    /// Audit trail of all approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
}

impl ApprovalManager {
    /// Create a new `ApprovalManager`.
    pub fn new(level: AutonomyLevel, interactive: bool) -> Self {
        Self {
            level,
            interactive,
            auto_approve: Mutex::new(HashSet::new()),
            always_ask: Mutex::new(HashSet::new()),
            session_allowlist: Mutex::new(HashSet::new()),
            non_cli_session_allowlist: Mutex::new(HashSet::new()),
            non_cli_allow_all_once: Mutex::new(0),
            pending_requests: Mutex::new(HashMap::new()),
            approval_timeout_minutes: 30,
            audit_log: Mutex::new(Vec::new()),
        }
    }

    /// Create from a config string (e.g. `"supervised"`).
    pub fn from_config_str(level_str: &str, interactive: bool) -> Self {
        Self::new(
            level_str.parse().unwrap_or(AutonomyLevel::Supervised),
            interactive,
        )
    }

    /// Add tools that never require approval in Supervised mode.
    pub fn with_auto_approve(self, tools: Vec<String>) -> Self {
        {
            let mut set = self.auto_approve.lock().unwrap();
            set.extend(tools);
        }
        self
    }

    /// Add tools that always require approval (overrides session allowlist).
    pub fn with_always_ask(self, tools: Vec<String>) -> Self {
        {
            let mut set = self.always_ask.lock().unwrap();
            set.extend(tools);
        }
        self
    }

    /// Set the timeout (minutes) for pending non-CLI approval requests.
    pub fn with_timeout_minutes(mut self, minutes: i64) -> Self {
        self.approval_timeout_minutes = minutes;
        self
    }

    // ── Core approval logic ───────────────────────────────────────────────────

    /// Check whether a tool requires interactive approval.
    ///
    /// Returns `false` for `Full` and `ReadOnly` autonomy levels (Full never
    /// prompts; ReadOnly blocks execution elsewhere, not via prompts).
    /// Returns `false` if the tool is in `auto_approve` or `session_allowlist`.
    /// Returns `true` for `Supervised` mode if none of the bypass conditions apply.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        if self.level != AutonomyLevel::Supervised {
            return false;
        }
        if self.always_ask.lock().unwrap().contains(tool_name) {
            return true;
        }
        if self.auto_approve.lock().unwrap().contains(tool_name) {
            return false;
        }
        if self.session_allowlist.lock().unwrap().contains(tool_name) {
            return false;
        }
        true
    }

    /// Request approval for a tool execution.
    ///
    /// # Decision matrix
    ///
    /// | Level      | Interactive | Condition                | Result              |
    /// |------------|-------------|--------------------------|---------------------|
    /// | ReadOnly   | any         | —                        | Denied              |
    /// | Full       | any         | —                        | Approved            |
    /// | Supervised | any         | in `auto_approve`        | Approved            |
    /// | Supervised | any         | not in `always_ask` + in session/non_cli allowlist | Approved |
    /// | Supervised | any         | `allow_all_once` token   | Approved (consumed) |
    /// | Supervised | true (CLI)  | default                  | Prompt Y/N/A        |
    /// | Supervised | false       | default                  | Denied + request hint |
    pub fn approve_tool(&self, tool_name: &str, args_preview: &str) -> ApprovalDecision {
        self.approve_tool_internal(tool_name, args_preview, None, None, None)
    }

    /// Like `approve_tool` but with sender/channel context for non-CLI channels.
    ///
    /// When the Supervised non-interactive path is reached, this creates a
    /// `PendingApprovalRequest` and embeds the request ID in the denial message,
    /// allowing the engine to forward an approval prompt to the channel.
    pub fn approve_tool_supervised(
        &self,
        tool_name: &str,
        args_preview: &str,
        sender: &str,
        channel: &str,
        reply_target: &str,
    ) -> ApprovalDecision {
        self.approve_tool_internal(
            tool_name,
            args_preview,
            Some(sender),
            Some(channel),
            Some(reply_target),
        )
    }

    fn approve_tool_internal(
        &self,
        tool_name: &str,
        args_preview: &str,
        sender: Option<&str>,
        channel: Option<&str>,
        reply_target: Option<&str>,
    ) -> ApprovalDecision {
        match &self.level {
            AutonomyLevel::ReadOnly => {
                warn!(tool = %tool_name, "Tool denied: ReadOnly mode");
                self.record_decision_str(tool_name, args_preview, false, "system");
                ApprovalDecision::Denied(
                    "ReadOnly mode: tool execution is not permitted. \
                     Change `security.autonomy_level` to 'supervised' or 'full' to enable tools."
                        .to_string(),
                )
            }

            AutonomyLevel::Full => {
                self.record_decision_str(tool_name, args_preview, true, "system");
                ApprovalDecision::Approved
            }

            AutonomyLevel::Supervised => {
                // 1. Check auto_approve (tools that never need approval)
                if self.auto_approve.lock().unwrap().contains(tool_name) {
                    self.record_decision_str(tool_name, args_preview, true, "auto_approve");
                    return ApprovalDecision::Approved;
                }

                let in_always_ask = self.always_ask.lock().unwrap().contains(tool_name);

                // 2. Check allowlists (skipped if tool is in always_ask)
                if !in_always_ask {
                    // 2a. One-time "allow all" bypass token
                    if self.consume_non_cli_allow_all_once() {
                        self.record_decision_str(
                            tool_name,
                            args_preview,
                            true,
                            "allow_all_once",
                        );
                        return ApprovalDecision::Approved;
                    }

                    // 2b. CLI session allowlist (from prior "Always" responses)
                    if self.session_allowlist.lock().unwrap().contains(tool_name) {
                        self.record_decision_str(
                            tool_name,
                            args_preview,
                            true,
                            "session_allowlist",
                        );
                        return ApprovalDecision::Approved;
                    }

                    // 2c. Non-CLI explicit session grant
                    if self
                        .non_cli_session_allowlist
                        .lock()
                        .unwrap()
                        .contains(tool_name)
                    {
                        self.record_decision_str(
                            tool_name,
                            args_preview,
                            true,
                            "non_cli_session",
                        );
                        return ApprovalDecision::Approved;
                    }
                }

                // 3. Interactive CLI prompt
                if self.interactive {
                    let decision = self.prompt_interactive(tool_name, args_preview);
                    match &decision {
                        ApprovalDecision::Approved => {
                            self.record_decision_str(tool_name, args_preview, true, "cli");
                        }
                        ApprovalDecision::Denied(_) => {
                            self.record_decision_str(tool_name, args_preview, false, "cli");
                        }
                    }
                    decision
                } else {
                    // 4. Non-interactive: create pending request if context is available
                    let channel_name = channel.unwrap_or("unknown");
                    if let (Some(s), Some(c), Some(r)) = (sender, channel, reply_target) {
                        let req = self.create_pending_request(tool_name, args_preview, s, c, r);
                        warn!(
                            tool = %tool_name,
                            request_id = %req.request_id,
                            channel = %c,
                            "Tool requires approval: pending request created"
                        );
                        self.record_decision_str(tool_name, args_preview, false, channel_name);
                        ApprovalDecision::Denied(format!(
                            "Supervised mode: tool '{}' requires human confirmation. \
                             Approval request sent (ID: {}). \
                             Press ✅ Approve in the chat or run `/approve-allow {}` to authorize, \
                             then retry your request.",
                            tool_name, req.request_id, req.request_id
                        ))
                    } else {
                        warn!(
                            tool = %tool_name,
                            "Tool auto-denied: Supervised mode on non-interactive channel (no context)"
                        );
                        self.record_decision_str(tool_name, args_preview, false, channel_name);
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
    }

    // ── Interactive CLI prompt ────────────────────────────────────────────────

    /// Display a tool call prompt and wait for user input (Y/N/A).
    /// "A" (Always) adds the tool to the session allowlist for future calls.
    fn prompt_interactive(&self, tool_name: &str, args_preview: &str) -> ApprovalDecision {
        println!();
        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║  🔧  TOOL CALL REQUEST                                       ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!("  Tool : {}", tool_name);

        let args_display = summarize_args(args_preview, 200);
        println!("  Args : {}", args_display);
        println!();
        print!("  Allow execution? [y]es / [n]o / [a]lways (don't ask again): ");
        io::stdout().flush().unwrap_or(());

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(_) => {
                let trimmed = input.trim().to_lowercase();
                match trimmed.as_str() {
                    "y" | "yes" => {
                        println!("  ✅ Approved.\n");
                        ApprovalDecision::Approved
                    }
                    "a" | "always" => {
                        // Add to session allowlist — subsequent calls for this tool are auto-approved
                        {
                            let mut allowlist = self.session_allowlist.lock().unwrap();
                            allowlist.insert(tool_name.to_string());
                        }
                        println!("  ✅ Approved (always — added to session allowlist).\n");
                        ApprovalDecision::Approved
                    }
                    _ => {
                        println!("  ❌ Denied.\n");
                        ApprovalDecision::Denied(format!(
                            "Tool '{}' was denied by the user.",
                            tool_name
                        ))
                    }
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

    // ── Session allowlist ─────────────────────────────────────────────────────

    /// Returns a snapshot of the current CLI session allowlist.
    /// Tools in this set were approved with "Always" in the current CLI session.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().unwrap().clone()
    }

    // ── Non-CLI session allowlist ─────────────────────────────────────────────

    /// Grant non-CLI session approval for a specific tool.
    ///
    /// Called when the user presses "✅ Approve" on a Telegram inline keyboard
    /// or sends a `/approve-allow <tool>` command.
    pub fn grant_non_cli_session(&self, tool_name: &str) {
        let mut allowlist = self.non_cli_session_allowlist.lock().unwrap();
        allowlist.insert(tool_name.to_string());
    }

    /// Revoke non-CLI session approval for a specific tool.
    ///
    /// Returns `true` if the tool was in the allowlist, `false` otherwise.
    pub fn revoke_non_cli_session(&self, tool_name: &str) -> bool {
        let mut allowlist = self.non_cli_session_allowlist.lock().unwrap();
        allowlist.remove(tool_name)
    }

    /// Check whether non-CLI session approval exists for a tool.
    pub fn is_non_cli_session_granted(&self, tool_name: &str) -> bool {
        self.non_cli_session_allowlist
            .lock()
            .unwrap()
            .contains(tool_name)
    }

    /// Returns a snapshot of the current non-CLI session allowlist.
    pub fn non_cli_session_allowlist(&self) -> HashSet<String> {
        self.non_cli_session_allowlist.lock().unwrap().clone()
    }

    // ── One-time "allow all" token ────────────────────────────────────────────

    /// Grant one "allow all tools for one turn" bypass token.
    ///
    /// Used when the user sends a "Yes, run everything" response on a non-CLI channel.
    /// Returns the new remaining token count.
    pub fn grant_non_cli_allow_all_once(&self) -> u32 {
        let mut remaining = self.non_cli_allow_all_once.lock().unwrap();
        *remaining = remaining.saturating_add(1);
        *remaining
    }

    /// Consume one "allow all" bypass token.
    ///
    /// Returns `true` if a token was consumed; `false` if none remained.
    pub fn consume_non_cli_allow_all_once(&self) -> bool {
        let mut remaining = self.non_cli_allow_all_once.lock().unwrap();
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        true
    }

    /// Returns the number of remaining one-time bypass tokens.
    pub fn non_cli_allow_all_once_remaining(&self) -> u32 {
        *self.non_cli_allow_all_once.lock().unwrap()
    }

    // ── Auto-approve / always-ask runtime policy ──────────────────────────────

    /// Add a tool to the runtime `auto_approve` list and remove it from `always_ask`.
    ///
    /// Called when the operator runs `/approve-allow <tool>` with a "persistent" flag.
    pub fn apply_auto_approve(&self, tool_name: &str) {
        {
            let mut auto = self.auto_approve.lock().unwrap();
            auto.insert(tool_name.to_string());
        }
        let mut always = self.always_ask.lock().unwrap();
        always.remove(tool_name);
    }

    /// Remove a tool from the runtime `auto_approve` list.
    ///
    /// Returns `true` if the tool was present.
    pub fn apply_auto_approve_revoke(&self, tool_name: &str) -> bool {
        let mut auto = self.auto_approve.lock().unwrap();
        auto.remove(tool_name)
    }

    /// Returns a snapshot of the current `auto_approve` tool set.
    pub fn auto_approve_tools(&self) -> HashSet<String> {
        self.auto_approve.lock().unwrap().clone()
    }

    /// Returns a snapshot of the current `always_ask` tool set.
    pub fn always_ask_tools(&self) -> HashSet<String> {
        self.always_ask.lock().unwrap().clone()
    }

    // ── Pending approval requests ─────────────────────────────────────────────

    /// Create a pending approval request for a non-CLI channel.
    ///
    /// If an identical request (same tool + sender + channel + reply_target) already exists
    /// and has not expired, the existing request is returned (deduplication).
    pub fn create_pending_request(
        &self,
        tool_name: &str,
        args_preview: &str,
        requested_by: &str,
        requested_channel: &str,
        requested_reply_target: &str,
    ) -> PendingApprovalRequest {
        let mut pending = self.pending_requests.lock().unwrap();
        prune_expired(&mut pending);

        // Dedup: return existing active request for the same context
        if let Some(existing) = pending.values().find(|req| {
            req.tool_name == tool_name
                && req.requested_by == requested_by
                && req.requested_channel == requested_channel
                && req.requested_reply_target == requested_reply_target
        }) {
            return existing.clone();
        }

        let now = Utc::now();
        let expires = now + Duration::minutes(self.approval_timeout_minutes);

        // Ensure uniqueness of the ID
        let mut request_id = generate_request_id();
        while pending.contains_key(&request_id) {
            request_id = generate_request_id();
        }

        let summary = summarize_args(args_preview, 100);
        let req = PendingApprovalRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.to_string(),
            args_preview: summary,
            requested_by: requested_by.to_string(),
            requested_channel: requested_channel.to_string(),
            requested_reply_target: requested_reply_target.to_string(),
            created_at: now.to_rfc3339(),
            expires_at: expires.to_rfc3339(),
        };

        pending.insert(request_id, req.clone());
        req
    }

    /// Confirm a pending approval request (Approve button pressed / `/approve-allow` command).
    ///
    /// - The confirming sender, channel, and reply_target must match the original request.
    /// - On success, removes the request from the pending map.
    /// - **Does NOT automatically grant session approval** — callers should call
    ///   `grant_non_cli_session(tool_name)` after a successful confirmation.
    pub fn confirm_pending_request(
        &self,
        request_id: &str,
        confirmed_by: &str,
        confirmed_channel: &str,
        confirmed_reply_target: &str,
    ) -> Result<PendingApprovalRequest, PendingApprovalError> {
        let mut pending = self.pending_requests.lock().unwrap();
        prune_expired(&mut pending);

        let req = pending
            .remove(request_id)
            .ok_or(PendingApprovalError::NotFound)?;

        if is_expired(&req) {
            return Err(PendingApprovalError::Expired);
        }

        if req.requested_by != confirmed_by
            || req.requested_channel != confirmed_channel
            || req.requested_reply_target != confirmed_reply_target
        {
            // Re-insert and return mismatch error
            pending.insert(req.request_id.clone(), req);
            return Err(PendingApprovalError::RequesterMismatch);
        }

        Ok(req)
    }

    /// Reject a pending approval request (Deny button pressed / `/approve-deny` command).
    ///
    /// The rejecting sender, channel, and reply_target must match the original request.
    pub fn reject_pending_request(
        &self,
        request_id: &str,
        rejected_by: &str,
        rejected_channel: &str,
        rejected_reply_target: &str,
    ) -> Result<PendingApprovalRequest, PendingApprovalError> {
        let mut pending = self.pending_requests.lock().unwrap();
        prune_expired(&mut pending);

        let req = pending
            .remove(request_id)
            .ok_or(PendingApprovalError::NotFound)?;

        if is_expired(&req) {
            return Err(PendingApprovalError::Expired);
        }

        if req.requested_by != rejected_by
            || req.requested_channel != rejected_channel
            || req.requested_reply_target != rejected_reply_target
        {
            pending.insert(req.request_id.clone(), req);
            return Err(PendingApprovalError::RequesterMismatch);
        }

        Ok(req)
    }

    /// Returns `true` if a non-expired pending request with this ID exists.
    pub fn has_pending_request(&self, request_id: &str) -> bool {
        let mut pending = self.pending_requests.lock().unwrap();
        prune_expired(&mut pending);
        pending.contains_key(request_id)
    }

    /// List active pending requests, optionally filtered by sender and/or channel.
    pub fn list_pending_requests(
        &self,
        requested_by: Option<&str>,
        requested_channel: Option<&str>,
    ) -> Vec<PendingApprovalRequest> {
        let mut pending = self.pending_requests.lock().unwrap();
        prune_expired(&mut pending);
        let mut rows: Vec<PendingApprovalRequest> = pending
            .values()
            .filter(|req| {
                requested_by.map_or(true, |by| req.requested_by == by)
                    && requested_channel
                        .map_or(true, |ch| req.requested_channel == ch)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        rows
    }

    /// Remove all pending requests for a specific tool.
    pub fn clear_pending_for_tool(&self, tool_name: &str) -> usize {
        let mut pending = self.pending_requests.lock().unwrap();
        let before = pending.len();
        pending.retain(|_, req| req.tool_name != tool_name);
        before.saturating_sub(pending.len())
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    /// Returns a snapshot of the approval audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().unwrap().clone()
    }

    fn record_decision_str(
        &self,
        tool_name: &str,
        args_preview: &str,
        approved: bool,
        channel: &str,
    ) {
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summarize_args(args_preview, 80),
            approved,
            channel: channel.to_string(),
        };
        let mut log = self.audit_log.lock().unwrap();
        // Bound the in-memory log to prevent unbounded growth
        if log.len() >= 10_000 {
            log.drain(0..1_000);
        }
        log.push(entry);
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Backward-compatible tests (existing behavior) ─────────────────────────

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
        assert_eq!(
            "readonly".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::ReadOnly
        );
        assert_eq!(
            "ReadOnly".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::ReadOnly
        );
        assert_eq!(
            "read_only".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::ReadOnly
        );
        assert_eq!(
            "full".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::Full
        );
        assert_eq!(
            "FULL".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::Full
        );
        assert_eq!(
            "supervised".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::Supervised
        );
        assert_eq!(
            "unknown".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::Supervised
        );
        assert_eq!(
            "".parse::<AutonomyLevel>().unwrap(),
            AutonomyLevel::Supervised
        );
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

    // ── auto_approve / always_ask ─────────────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_auto_approve(vec!["file_read".into(), "memory_recall".into()]);
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
        // auto_approve overrides the non-interactive default
        assert!(mgr.approve_tool("file_read", "path.txt").is_approved());
        assert!(mgr.approve_tool("memory_recall", "key").is_approved());
    }

    #[test]
    fn always_ask_tools_always_need_approval() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_always_ask(vec!["shell".into()]);
        assert!(mgr.needs_approval("shell"));
        // Even with a session grant, always_ask wins
        mgr.grant_non_cli_session("shell");
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn always_ask_overrides_non_cli_session_grant() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_always_ask(vec!["shell".into()]);
        mgr.grant_non_cli_session("shell");
        let d = mgr.approve_tool("shell", "ls");
        assert!(!d.is_approved());
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn full_autonomy_never_needs_approval() {
        let mgr = ApprovalManager::new(AutonomyLevel::Full, false);
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn readonly_never_needs_approval_prompt() {
        let mgr = ApprovalManager::new(AutonomyLevel::ReadOnly, false);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── Non-CLI session allowlist ─────────────────────────────────────────────

    #[test]
    fn non_cli_session_grant_allows_tool() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        assert!(!mgr.is_non_cli_session_granted("shell"));
        mgr.grant_non_cli_session("shell");
        assert!(mgr.is_non_cli_session_granted("shell"));
        // approve_tool should now return Approved
        assert!(mgr.approve_tool("shell", "ls").is_approved());
    }

    #[test]
    fn non_cli_session_revoke_removes_grant() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        mgr.grant_non_cli_session("shell");
        assert!(mgr.revoke_non_cli_session("shell"));
        assert!(!mgr.is_non_cli_session_granted("shell"));
        // Second revoke returns false
        assert!(!mgr.revoke_non_cli_session("shell"));
    }

    #[test]
    fn non_cli_session_allowlist_snapshot() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        mgr.grant_non_cli_session("shell");
        mgr.grant_non_cli_session("file_write");
        let snapshot = mgr.non_cli_session_allowlist();
        assert!(snapshot.contains("shell"));
        assert!(snapshot.contains("file_write"));
    }

    // ── One-time bypass token ─────────────────────────────────────────────────

    #[test]
    fn allow_all_once_token_lifecycle() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        assert_eq!(mgr.non_cli_allow_all_once_remaining(), 0);
        assert!(!mgr.consume_non_cli_allow_all_once());

        assert_eq!(mgr.grant_non_cli_allow_all_once(), 1);
        assert_eq!(mgr.grant_non_cli_allow_all_once(), 2);
        assert_eq!(mgr.non_cli_allow_all_once_remaining(), 2);

        // First consume
        assert!(mgr.approve_tool("shell", "ls").is_approved());
        assert_eq!(mgr.non_cli_allow_all_once_remaining(), 1);

        // Second consume via direct call
        assert!(mgr.consume_non_cli_allow_all_once());
        assert_eq!(mgr.non_cli_allow_all_once_remaining(), 0);
        assert!(!mgr.consume_non_cli_allow_all_once());
    }

    #[test]
    fn allow_all_once_does_not_bypass_always_ask() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_always_ask(vec!["shell".into()]);
        mgr.grant_non_cli_allow_all_once();
        // always_ask takes priority over allow_all_once
        let d = mgr.approve_tool("shell", "ls");
        assert!(!d.is_approved(), "always_ask must override allow_all_once");
        // Token should still be consumed (allow_all_once check happens before always_ask check in deny path)
        // Actually per the implementation, always_ask is checked first, so token is NOT consumed.
        // Let's verify the token was NOT consumed (always_ask short-circuits before reaching the token check)
        // The remaining token count depends on implementation ordering.
        // In our impl, always_ask is checked FIRST (step 1), then auto_approve (still step 1),
        // then we check "if !in_always_ask" before the token. So token is NOT consumed.
        assert_eq!(mgr.non_cli_allow_all_once_remaining(), 1, "token preserved since always_ask blocked early");
    }

    // ── Runtime auto_approve policy ───────────────────────────────────────────

    #[test]
    fn apply_auto_approve_updates_policy() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_always_ask(vec!["shell".into()]);
        assert!(mgr.needs_approval("shell"));

        mgr.apply_auto_approve("shell");
        assert!(!mgr.needs_approval("shell"));
        assert!(mgr.auto_approve_tools().contains("shell"));
        assert!(!mgr.always_ask_tools().contains("shell"));
    }

    #[test]
    fn apply_auto_approve_revoke_updates_policy() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_auto_approve(vec!["file_read".into()]);
        assert!(!mgr.needs_approval("file_read"));

        assert!(mgr.apply_auto_approve_revoke("file_read"));
        assert!(mgr.needs_approval("file_read"));
        assert!(!mgr.apply_auto_approve_revoke("file_read"));
    }

    // ── Pending approval requests ─────────────────────────────────────────────

    #[test]
    fn create_and_confirm_pending_request() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req = mgr.create_pending_request("shell", "ls -la", "alice", "telegram", "chat-1");
        assert_eq!(req.tool_name, "shell");
        assert!(req.request_id.starts_with("apr-"));
        assert!(mgr.has_pending_request(&req.request_id));

        let confirmed = mgr
            .confirm_pending_request(&req.request_id, "alice", "telegram", "chat-1")
            .expect("confirm should succeed");
        assert_eq!(confirmed.request_id, req.request_id);
        // Now gone
        assert!(!mgr.has_pending_request(&req.request_id));
    }

    #[test]
    fn create_and_reject_pending_request() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req = mgr.create_pending_request("shell", "ls -la", "alice", "telegram", "chat-1");

        let rejected = mgr
            .reject_pending_request(&req.request_id, "alice", "telegram", "chat-1")
            .expect("reject should succeed");
        assert_eq!(rejected.request_id, req.request_id);
        assert!(!mgr.has_pending_request(&req.request_id));
    }

    #[test]
    fn pending_request_requester_mismatch_rejected() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req = mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");

        // Wrong sender
        let err = mgr
            .confirm_pending_request(&req.request_id, "bob", "telegram", "chat-1")
            .expect_err("should fail with mismatch");
        assert_eq!(err, PendingApprovalError::RequesterMismatch);

        // Wrong channel
        let err = mgr
            .confirm_pending_request(&req.request_id, "alice", "discord", "chat-1")
            .expect_err("should fail with mismatch");
        assert_eq!(err, PendingApprovalError::RequesterMismatch);

        // Wrong reply target
        let err = mgr
            .confirm_pending_request(&req.request_id, "alice", "telegram", "chat-2")
            .expect_err("should fail with mismatch");
        assert_eq!(err, PendingApprovalError::RequesterMismatch);

        // Original request still present
        assert!(mgr.has_pending_request(&req.request_id));
    }

    #[test]
    fn pending_request_not_found_after_consume() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req = mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");
        mgr.confirm_pending_request(&req.request_id, "alice", "telegram", "chat-1")
            .unwrap();
        let err = mgr
            .confirm_pending_request(&req.request_id, "alice", "telegram", "chat-1")
            .expect_err("second confirm should fail");
        assert_eq!(err, PendingApprovalError::NotFound);
    }

    #[test]
    fn pending_request_dedup_returns_same_request() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req1 =
            mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");
        let req2 =
            mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");
        // Should be same request ID (dedup)
        assert_eq!(req1.request_id, req2.request_id);
    }

    #[test]
    fn pending_request_expired_is_pruned() {
        use chrono::Duration;
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let req = mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");

        // Manually expire the request
        {
            let mut pending = mgr.pending_requests.lock().unwrap();
            let row = pending.get_mut(&req.request_id).unwrap();
            row.expires_at = (Utc::now() - Duration::minutes(1)).to_rfc3339();
        }

        // has_pending_request should return false (prunes expired)
        assert!(!mgr.has_pending_request(&req.request_id));
        assert_eq!(
            mgr.confirm_pending_request(&req.request_id, "alice", "telegram", "chat-1")
                .unwrap_err(),
            PendingApprovalError::NotFound
        );
    }

    #[test]
    fn list_pending_requests_filters_correctly() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");
        mgr.create_pending_request("file_write", "data.txt", "bob", "telegram", "chat-1");
        mgr.create_pending_request("shell", "pwd", "alice", "discord", "guild-1");

        let alice_telegram = mgr.list_pending_requests(Some("alice"), Some("telegram"));
        assert_eq!(alice_telegram.len(), 1);
        assert_eq!(alice_telegram[0].tool_name, "shell");

        let telegram_all = mgr.list_pending_requests(None, Some("telegram"));
        assert_eq!(telegram_all.len(), 2);

        let alice_all = mgr.list_pending_requests(Some("alice"), None);
        assert_eq!(alice_all.len(), 2);
    }

    #[test]
    fn approve_tool_supervised_with_context_creates_pending_request() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        let d = mgr.approve_tool_supervised("shell", "ls -la", "alice", "telegram", "chat-1");
        assert!(!d.is_approved());
        let reason = d.denial_reason().unwrap();
        assert!(reason.contains("apr-"), "denial message should include request ID");
        // A pending request should have been created
        let pending = mgr.list_pending_requests(Some("alice"), Some("telegram"));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_name, "shell");
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false)
            .with_auto_approve(vec!["file_read".into()]);

        mgr.approve_tool("file_read", "test.txt");
        mgr.approve_tool("shell", "ls");

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "file_read");
        assert!(log[0].approved);
        assert_eq!(log[1].tool_name, "shell");
        assert!(!log[1].approved);
    }

    #[test]
    fn audit_log_contains_timestamp() {
        let mgr = ApprovalManager::new(AutonomyLevel::Full, false);
        mgr.approve_tool("shell", "echo test");
        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
    }

    // ── Clear pending for tool ────────────────────────────────────────────────

    #[test]
    fn clear_pending_for_tool_removes_matching_requests() {
        let mgr = ApprovalManager::new(AutonomyLevel::Supervised, false);
        mgr.create_pending_request("shell", "ls", "alice", "telegram", "chat-1");
        mgr.create_pending_request("shell", "pwd", "bob", "telegram", "chat-2");
        mgr.create_pending_request("file_write", "x.txt", "alice", "telegram", "chat-1");

        let removed = mgr.clear_pending_for_tool("shell");
        assert_eq!(removed, 2);
        assert_eq!(mgr.list_pending_requests(None, None).len(), 1);
        assert_eq!(
            mgr.list_pending_requests(None, None)[0].tool_name,
            "file_write"
        );
    }
}
