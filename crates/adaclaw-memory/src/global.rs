/// GlobalMemory — cross-agent shared reference memory.
///
/// ## Purpose
///
/// In a multi-agent setup every agent has its own private memory, but some
/// knowledge should be shared across *all* agents:
///
/// - User's name, timezone, language preference
/// - Project-level facts ("our stack is Rust + axum")
/// - Standing instructions ("always respond in Chinese")
/// - Long-lived reference data that never changes session-to-session
///
/// `GlobalMemory` is a transparent wrapper around any `Memory` backend.  It
/// intercepts `recall()` to **prepend global entries** (those stored with
/// `Category::Global`) before the agent's private results, so every agent
/// automatically sees shared knowledge without any extra configuration.
///
/// ## Convention
///
/// - **Reading**: any agent can call `recall()` and will receive global entries
///   ranked first (by a small score boost in the RRF merge step).
/// - **Writing**: only explicit `memory_store global=true` calls should write
///   `Category::Global` entries; agents should not write global entries
///   autonomously.
///
/// ## RecallScope behaviour
///
/// `GlobalMemory` passes the scope through to the inner backend.
/// `Category::Global` entries are treated like `Core` — they are included in
/// all scopes except `Clean`, and they are never topic-filtered (topic is
/// always `None` for Global entries).
use adaclaw_core::memory::{Category, Memory, MemoryEntry, RecallScope};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;

pub struct GlobalMemory {
    /// The underlying memory backend (same instance shared across all agents).
    inner: Arc<dyn Memory>,
}

impl GlobalMemory {
    /// Wrap an existing `Memory` backend.
    pub fn new(inner: Arc<dyn Memory>) -> Self {
        Self { inner }
    }

    /// Store a **global** knowledge entry visible to all agents.
    ///
    /// Equivalent to `store(key, content, Category::Global, None, None)` but
    /// provides a clearer intent at the call site.
    pub async fn store_global(&self, key: &str, content: &str) -> Result<()> {
        self.inner
            .store(key, content, Category::Global, None, None)
            .await
    }

    /// List all global entries (for inspection / management).
    pub async fn list_global(&self) -> Result<Vec<MemoryEntry>> {
        self.inner.list(Some(&Category::Global), None).await
    }
}

#[async_trait]
impl Memory for GlobalMemory {
    fn name(&self) -> &str {
        "global"
    }

    // ── store ─────────────────────────────────────────────────────────────────

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: Category,
        session: Option<&str>,
        topic: Option<&str>,
    ) -> Result<()> {
        self.inner
            .store(key, content, category, session, topic)
            .await
    }

    // ── recall ────────────────────────────────────────────────────────────────

    /// Recall entries from the underlying backend.
    ///
    /// Global entries (`Category::Global`) are fetched in a separate pass and
    /// **prepended** to the result, then the combined list is deduped and
    /// truncated to `limit`.
    ///
    /// When `scope == Clean`, returns empty immediately (no global entries
    /// either — the user explicitly asked for a clean slate).
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
        scope: RecallScope,
    ) -> Result<Vec<MemoryEntry>> {
        // Clean scope: return nothing at all
        if scope == RecallScope::Clean {
            return Ok(vec![]);
        }

        // Step 1: Fetch global entries (not session-scoped, not topic-filtered).
        // Global entries are treated as Core-equivalent: always included in
        // FactsOnly and CurrentTopic scopes.
        let global_results = self
            .inner
            .recall(query, limit, None, RecallScope::Full)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.category == Category::Global)
            .collect::<Vec<_>>();

        // Step 2: Fetch session/topic-scoped private entries using the requested scope.
        // Exclude Global so we don't double-count.
        let private_results = self
            .inner
            .recall(query, limit, session, scope)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.category != Category::Global)
            .collect::<Vec<_>>();

        // Step 3: Merge — globals first, then private, dedup by key.
        let mut seen: HashSet<String> = HashSet::new();
        let mut merged: Vec<MemoryEntry> = Vec::with_capacity(limit);

        for entry in global_results.into_iter().chain(private_results) {
            if merged.len() >= limit {
                break;
            }
            if seen.insert(entry.key.clone()) {
                merged.push(entry);
            }
        }

        Ok(merged)
    }

    // ── get ───────────────────────────────────────────────────────────────────

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        self.inner.get(key).await
    }

    // ── list ──────────────────────────────────────────────────────────────────

    async fn list(
        &self,
        category: Option<&Category>,
        session: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        self.inner.list(category, session).await
    }

    // ── forget ────────────────────────────────────────────────────────────────

    async fn forget(&self, key: &str) -> Result<bool> {
        self.inner.forget(key).await
    }

    // ── count ─────────────────────────────────────────────────────────────────

    async fn count(&self) -> Result<usize> {
        self.inner.count().await
    }

    // ── health_check ──────────────────────────────────────────────────────────

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteMemory;

    fn make_global() -> GlobalMemory {
        let inner: Arc<dyn Memory> = Arc::new(SqliteMemory::new());
        GlobalMemory::new(inner)
    }

    #[tokio::test]
    async fn test_global_entries_prepended_in_recall() {
        let gm = make_global();

        // Store a private entry
        gm.store(
            "priv1",
            "private deployment notes",
            Category::Daily,
            Some("s1"),
            None,
        )
        .await
        .unwrap();

        // Store a global entry
        gm.store_global("glob1", "user prefers English replies")
            .await
            .unwrap();

        let results = gm
            .recall("user", 10, Some("s1"), RecallScope::Full)
            .await
            .unwrap();
        assert!(!results.is_empty());
        let first = &results[0];
        assert_eq!(first.category, Category::Global);
    }

    #[tokio::test]
    async fn test_store_global_helper() {
        let gm = make_global();
        gm.store_global("pref:language", "always reply in Chinese")
            .await
            .unwrap();

        let entry = gm.get("pref:language").await.unwrap().unwrap();
        assert_eq!(entry.category, Category::Global);
        assert_eq!(entry.content, "always reply in Chinese");
    }

    #[tokio::test]
    async fn test_list_global() {
        let gm = make_global();
        gm.store_global("g1", "global fact one").await.unwrap();
        gm.store_global("g2", "global fact two").await.unwrap();
        gm.store("p1", "private", Category::Daily, None, None)
            .await
            .unwrap();

        let globals = gm.list_global().await.unwrap();
        assert_eq!(globals.len(), 2);
        assert!(globals.iter().all(|e| e.category == Category::Global));
    }

    #[tokio::test]
    async fn test_dedup_in_recall() {
        let gm = make_global();
        gm.store_global("shared_key", "this is global knowledge")
            .await
            .unwrap();

        let results = gm
            .recall("global knowledge", 20, None, RecallScope::Full)
            .await
            .unwrap();
        let count = results.iter().filter(|e| e.key == "shared_key").count();
        assert_eq!(count, 1, "global key must not be duplicated in results");
    }

    #[tokio::test]
    async fn test_clean_scope_returns_empty() {
        let gm = make_global();
        gm.store_global("g1", "some global fact").await.unwrap();

        let results = gm
            .recall("global", 10, None, RecallScope::Clean)
            .await
            .unwrap();
        assert!(
            results.is_empty(),
            "Clean scope must return nothing, even Global entries"
        );
    }

    #[tokio::test]
    async fn test_facts_only_still_returns_global() {
        let gm = make_global();
        gm.store_global("g1", "some global fact about fox")
            .await
            .unwrap();
        gm.store(
            "conv1",
            "conversation about fox",
            Category::Conversation,
            Some("s1"),
            Some("t1"),
        )
        .await
        .unwrap();

        let results = gm
            .recall("fox", 10, Some("s1"), RecallScope::FactsOnly)
            .await
            .unwrap();
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(
            keys.contains(&"g1"),
            "Global entries should be in FactsOnly results"
        );
        assert!(
            !keys.contains(&"conv1"),
            "Conversation should not be in FactsOnly results"
        );
    }

    #[tokio::test]
    async fn test_health_check() {
        let gm = make_global();
        assert!(gm.health_check().await);
    }
}
