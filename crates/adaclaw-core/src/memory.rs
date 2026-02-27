use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ── Category ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Category {
    /// Long-lived facts the user explicitly wants the agent to always know.
    Core,
    /// Ephemeral, day-to-day working notes.
    Daily,
    /// Indexed conversation turns (written automatically by AgentEngine).
    Conversation,
    /// Global shared knowledge visible to ALL agents (user preferences,
    /// project-level facts, etc.).  Read-only for agents by convention —
    /// written only via explicit `memory_store global=true` calls.
    Global,
    /// Arbitrary user-defined category label.
    Custom(String),
}

// ── RecallScope ───────────────────────────────────────────────────────────────

/// Controls which memory entries are returned by `recall()`.
///
/// This is the primary mechanism for topic isolation and clean-slate thinking.
///
/// ## Scope semantics
///
/// | Category    | Full | FactsOnly | CurrentTopic | Clean |
/// |-------------|------|-----------|--------------|-------|
/// | Core        | ✓    | ✓         | ✓            | ✗     |
/// | Global      | ✓    | ✓         | ✓            | ✗     |
/// | Daily       | ✓    | ✓         | ✓            | ✗     |
/// | Conversation| ✓    | ✗         | topic only   | ✗     |
///
/// Note: `Daily` entries are included in `FactsOnly` because they represent
/// working facts (e.g., "user is working on project X"), not conversation noise.
/// To suppress Daily too, use `Clean`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallScope {
    /// Normal retrieval — all categories, all topics.
    Full,
    /// Only Core, Global, Daily — no Conversation history.
    /// Triggered when topic similarity is medium (0.3–0.6) or user expresses
    /// intent to think without prior conversation context.
    FactsOnly,
    /// Core/Global/Daily from any topic, plus Conversation only from the
    /// specified topic_id.  Triggered on automatic topic switch (similarity < 0.3).
    CurrentTopic { topic_id: String },
    /// Return nothing — no memory injection at all.
    /// Triggered by explicit "clean slate" intent from the user.
    Clean,
}

// ── MemoryEntry ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub content: String,
    pub category: Category,
    /// Session ID — scopes Conversation entries to a specific user/session.
    pub session: Option<String>,
    /// Topic ID — scopes Conversation entries to a specific topic thread.
    /// `None` means globally applicable (always used for Core/Global/Daily).
    pub topic: Option<String>,
}

// ── Memory trait ──────────────────────────────────────────────────────────────

#[async_trait]
pub trait Memory: Send + Sync {
    fn name(&self) -> &str;

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: Category,
        session: Option<&str>,
        topic: Option<&str>,
    ) -> Result<()>;

    /// Retrieve entries matching `query` within the given `scope`.
    ///
    /// - `session` still scopes Conversation entries to a specific user/session.
    /// - `scope`   further controls which categories/topics are included.
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
        scope: RecallScope,
    ) -> Result<Vec<MemoryEntry>>;

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>>;

    async fn list(
        &self,
        category: Option<&Category>,
        session: Option<&str>,
    ) -> Result<Vec<MemoryEntry>>;

    async fn forget(&self, key: &str) -> Result<bool>;
    async fn count(&self) -> Result<usize>;
    async fn health_check(&self) -> bool;
}
