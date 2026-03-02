/// MarkdownMemory — file-based memory backend.
///
/// Each memory entry is stored as a Markdown file:
///   `{dir}/{key}.md`
///
/// File format:
/// ```markdown
/// ---
/// key: some-key
/// category: Daily
/// session: optional-session-id
/// topic: optional-topic-id
/// created_at: 1700000000
/// updated_at: 1700000000
/// ---
///
/// Entry content goes here.
/// ```
///
/// `recall()` performs a case-insensitive substring search across all files,
/// filtered by `RecallScope`.  There is no vector or FTS5 index — this backend
/// is intentionally lightweight and human-readable. For semantic search, use
/// `SqliteMemory` instead.
use adaclaw_core::memory::{Category, Memory, MemoryEntry, RecallScope};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub struct MarkdownMemory {
    dir: PathBuf,
}

impl MarkdownMemory {
    /// Create a `MarkdownMemory` that stores files in `dir`.
    /// The directory is created automatically if it does not exist.
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create memory directory: {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn key_to_path(&self, key: &str) -> PathBuf {
        // Sanitise: strip characters / sequences that could allow path traversal.
        let safe_key = key.replace(['/', '\\', '\0'], "_").replace("..", "__");
        let safe_key = safe_key.trim_matches(|c| c == '_' || c == '.').to_string();
        let safe_key = if safe_key.is_empty() {
            "_".to_string()
        } else {
            safe_key
        };
        self.dir.join(format!("{}.md", safe_key))
    }

    fn write_entry(&self, entry: &MemoryEntry) -> Result<()> {
        let cat_str = category_to_str(&entry.category);
        let now = unix_now();
        let session_line = match &entry.session {
            Some(s) => format!("session: {}\n", s),
            None => String::new(),
        };
        let topic_line = match &entry.topic {
            Some(t) => format!("topic: {}\n", t),
            None => String::new(),
        };

        let content = format!(
            "---\nkey: {key}\ncategory: {cat}\n{session}{topic}created_at: {ts}\nupdated_at: {ts}\n---\n\n{body}\n",
            key = entry.key,
            cat = cat_str,
            session = session_line,
            topic = topic_line,
            ts = now,
            body = entry.content,
        );

        std::fs::write(self.key_to_path(&entry.key), content)
            .with_context(|| format!("Failed to write memory entry: {}", entry.key))?;
        Ok(())
    }

    fn read_entry(&self, path: &Path) -> Option<MemoryEntry> {
        let raw = std::fs::read_to_string(path).ok()?;
        parse_markdown_entry(&raw)
    }

    fn all_entries(&self) -> Vec<MemoryEntry> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .filter_map(|e| self.read_entry(&e.path()))
            .collect()
    }
}

// ── Front-matter helpers ──────────────────────────────────────────────────────

fn parse_markdown_entry(raw: &str) -> Option<MemoryEntry> {
    let stripped = raw.strip_prefix("---\n")?;
    let (front, rest) = stripped.split_once("\n---\n")?;

    let mut key = String::new();
    let mut category = "Daily".to_string();
    let mut session: Option<String> = None;
    let mut topic: Option<String> = None;

    for line in front.lines() {
        if let Some(v) = line.strip_prefix("key: ") {
            key = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("category: ") {
            category = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("session: ") {
            let s = v.trim();
            if !s.is_empty() {
                session = Some(s.to_string());
            }
        } else if let Some(v) = line.strip_prefix("topic: ") {
            let t = v.trim();
            if !t.is_empty() {
                topic = Some(t.to_string());
            }
        }
    }

    if key.is_empty() {
        return None;
    }

    let content = rest
        .trim_start_matches('\n')
        .trim_end_matches('\n')
        .to_string();

    Some(MemoryEntry {
        key,
        content,
        category: str_to_category(&category),
        session,
        topic,
    })
}

fn category_to_str(cat: &Category) -> String {
    match cat {
        Category::Core => "Core".to_string(),
        Category::Daily => "Daily".to_string(),
        Category::Conversation => "Conversation".to_string(),
        Category::Global => "Global".to_string(),
        Category::Custom(s) => s.clone(),
    }
}

fn str_to_category(s: &str) -> Category {
    match s {
        "Core" => Category::Core,
        "Daily" => Category::Daily,
        "Conversation" => Category::Conversation,
        "Global" => Category::Global,
        other => Category::Custom(other.to_string()),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns true if an entry should be included given the scope.
fn entry_matches_scope(entry: &MemoryEntry, scope: &RecallScope) -> bool {
    match scope {
        RecallScope::Clean => false,
        RecallScope::Full => true,
        RecallScope::FactsOnly => {
            // Include everything except Conversation
            entry.category != Category::Conversation
        }
        RecallScope::CurrentTopic { topic_id } => {
            match entry.category {
                Category::Conversation => {
                    // Only include Conversation entries that match the current topic
                    entry.topic.as_deref() == Some(topic_id.as_str())
                }
                // Core / Global / Daily / Custom always included
                _ => true,
            }
        }
    }
}

// ── Memory trait impl ─────────────────────────────────────────────────────────

#[async_trait]
impl Memory for MarkdownMemory {
    fn name(&self) -> &str {
        "markdown"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: Category,
        session: Option<&str>,
        topic: Option<&str>,
    ) -> Result<()> {
        let entry = MemoryEntry {
            key: key.to_string(),
            content: content.to_string(),
            category,
            session: session.map(ToString::to_string),
            topic: topic.map(ToString::to_string),
        };
        self.write_entry(&entry)
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
        scope: RecallScope,
    ) -> Result<Vec<MemoryEntry>> {
        // Clean scope: return nothing
        if scope == RecallScope::Clean {
            return Ok(vec![]);
        }

        let query_lower = query.to_lowercase();
        let mut results: Vec<MemoryEntry> = self
            .all_entries()
            .into_iter()
            .filter(|e| {
                // Session filter (only for Conversation entries)
                if e.category == Category::Conversation
                    && let Some(sess) = session
                    && e.session.as_deref() != Some(sess)
                {
                    return false;
                }
                // Scope filter
                if !entry_matches_scope(e, &scope) {
                    return false;
                }
                // Content match
                e.content.to_lowercase().contains(&query_lower)
                    || e.key.to_lowercase().contains(&query_lower)
            })
            .collect();

        // Simple relevance: entries where query appears more often rank higher
        results.sort_by(|a, b| {
            let count = |e: &MemoryEntry| {
                let hay = e.content.to_lowercase();
                let mut count = 0usize;
                let mut pos = 0;
                while let Some(idx) = hay[pos..].find(&query_lower) {
                    count += 1;
                    pos += idx + query_lower.len();
                }
                count
            };
            count(b).cmp(&count(a))
        });

        results.truncate(limit);
        Ok(results)
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let path = self.key_to_path(key);
        if path.exists() {
            Ok(self.read_entry(&path))
        } else {
            Ok(None)
        }
    }

    async fn list(
        &self,
        category: Option<&Category>,
        session: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let entries = self
            .all_entries()
            .into_iter()
            .filter(|e| {
                if let Some(cat) = category
                    && &e.category != cat
                {
                    return false;
                }
                if let Some(sess) = session
                    && e.session.as_deref() != Some(sess)
                {
                    return false;
                }
                true
            })
            .collect();
        Ok(entries)
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        let path = self.key_to_path(key);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to delete memory entry: {}", key))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn count(&self) -> Result<usize> {
        let n = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                    .count()
            })
            .unwrap_or(0);
        Ok(n)
    }

    async fn health_check(&self) -> bool {
        self.dir.exists() && self.dir.is_dir()
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_mem() -> MarkdownMemory {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        MarkdownMemory::new(path).unwrap()
    }

    #[tokio::test]
    async fn test_store_and_get() {
        let mem = make_mem();
        mem.store("k1", "hello world", Category::Daily, None, None)
            .await
            .unwrap();
        let entry = mem.get("k1").await.unwrap().unwrap();
        assert_eq!(entry.content, "hello world");
        assert_eq!(entry.category, Category::Daily);
        assert_eq!(entry.topic, None);
    }

    #[tokio::test]
    async fn test_store_with_topic() {
        let mem = make_mem();
        mem.store(
            "k2",
            "topic content",
            Category::Conversation,
            Some("s1"),
            Some("topic-abc"),
        )
        .await
        .unwrap();
        let entry = mem.get("k2").await.unwrap().unwrap();
        assert_eq!(entry.topic, Some("topic-abc".to_string()));
    }

    #[tokio::test]
    async fn test_recall_full_scope() {
        let mem = make_mem();
        mem.store(
            "doc1",
            "the quick brown fox jumps",
            Category::Daily,
            None,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "doc2",
            "lazy dog sleeps soundly",
            Category::Daily,
            None,
            None,
        )
        .await
        .unwrap();

        let results = mem.recall("fox", 5, None, RecallScope::Full).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "doc1");
    }

    #[tokio::test]
    async fn test_recall_clean_scope_returns_empty() {
        let mem = make_mem();
        mem.store("k1", "some content", Category::Core, None, None)
            .await
            .unwrap();
        let results = mem
            .recall("content", 10, None, RecallScope::Clean)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_recall_facts_only_excludes_conversation() {
        let mem = make_mem();
        mem.store("core1", "core fact about fox", Category::Core, None, None)
            .await
            .unwrap();
        mem.store(
            "conv1",
            "conversation about fox",
            Category::Conversation,
            Some("s1"),
            Some("t1"),
        )
        .await
        .unwrap();

        let results = mem
            .recall("fox", 10, None, RecallScope::FactsOnly)
            .await
            .unwrap();
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"core1"), "core entries should be included");
        assert!(
            !keys.contains(&"conv1"),
            "conversation entries should be excluded"
        );
    }

    #[tokio::test]
    async fn test_recall_current_topic_filters_conversation() {
        let mem = make_mem();
        mem.store(
            "conv-t1",
            "rust topic conv",
            Category::Conversation,
            Some("s1"),
            Some("topic-rust"),
        )
        .await
        .unwrap();
        mem.store(
            "conv-t2",
            "poem haiku autumn",
            Category::Conversation,
            Some("s1"),
            Some("topic-poem"),
        )
        .await
        .unwrap();
        // Core entries have no session; markdown recall skips session-filter for
        // non-Conversation categories, so core1 is always reachable.
        // Use content that matches the query ("rust") so the content filter passes.
        mem.store("core1", "core fact about rust", Category::Core, None, None)
            .await
            .unwrap();

        let scope = RecallScope::CurrentTopic {
            topic_id: "topic-rust".to_string(),
        };
        // Query "rust": matches conv-t1 and core1, but NOT conv-t2 ("poem haiku autumn")
        let results = mem.recall("rust", 10, Some("s1"), scope).await.unwrap();
        let keys: Vec<&str> = results.iter().map(|e| e.key.as_str()).collect();
        assert!(
            keys.contains(&"conv-t1"),
            "rust topic conv should be included"
        );
        assert!(
            !keys.contains(&"conv-t2"),
            "poem topic conv should be excluded"
        );
        assert!(keys.contains(&"core1"), "core entries always included");
    }

    #[tokio::test]
    async fn test_forget() {
        let mem = make_mem();
        mem.store("k2", "delete me", Category::Core, None, None)
            .await
            .unwrap();
        assert!(mem.forget("k2").await.unwrap());
        assert!(mem.get("k2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_count() {
        let mem = make_mem();
        assert_eq!(mem.count().await.unwrap(), 0);
        mem.store("a", "aa", Category::Daily, None, None)
            .await
            .unwrap();
        mem.store("b", "bb", Category::Daily, None, None)
            .await
            .unwrap();
        assert_eq!(mem.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_session_filter() {
        let mem = make_mem();
        mem.store(
            "s1",
            "session one",
            Category::Conversation,
            Some("s1"),
            None,
        )
        .await
        .unwrap();
        mem.store(
            "s2",
            "session two",
            Category::Conversation,
            Some("s2"),
            None,
        )
        .await
        .unwrap();

        let r = mem
            .recall("session", 10, Some("s1"), RecallScope::Full)
            .await
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "s1");
    }

    #[test]
    fn test_key_sanitisation() {
        let mem = MarkdownMemory::new(std::path::Path::new("/tmp/test_md_mem")).unwrap();
        let path = mem.key_to_path("../../etc/passwd");
        assert!(!path.to_str().unwrap().contains(".."));
    }
}
