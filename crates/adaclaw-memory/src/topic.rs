/// TopicManager — automatic topic detection and switching.
///
/// ## How it works
///
/// After each user message, `check_and_switch()` compares the new message to the
/// currently active topic using one of two strategies:
///
/// **Strategy A — Embedding similarity (when embedder is available)**
/// The new message is embedded; cosine similarity is computed against the rolling
/// average embedding of recent messages in the current topic.
/// - similarity ≥ 0.6  → same topic, `Full` recall
/// - similarity 0.3–0.6 → related but drifted, `FactsOnly` recall
/// - similarity < 0.3   → topic switch, create/reuse topic_id, `CurrentTopic` recall
///
/// **Strategy B — Keyword overlap (fallback when no embedder)**
/// Compares normalised word sets from the last few messages vs. the new message.
/// - overlap ≥ 60%  → same topic
/// - overlap 30–60% → partial drift
/// - overlap < 30%  → topic switch
///
/// Both strategies are **silent**: no user-visible output, no prompts.
///
/// ## Topic reuse
///
/// Before creating a new topic_id, `TopicManager` searches the SQLite `topics`
/// table (via keyword similarity of the label) to reuse an existing one.  This
/// prevents topic fragmentation when users return to the same subject.
use adaclaw_core::memory::RecallScope;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Mutex;
use uuid::Uuid;

// ── Thresholds ────────────────────────────────────────────────────────────────

/// Above this similarity → same topic (Full recall, no context pruning).
const SAME_TOPIC_THRESHOLD: f32 = 0.60;
/// Above this similarity → related topic (FactsOnly recall, no pruning).
const RELATED_TOPIC_THRESHOLD: f32 = 0.30;
/// Reuse an existing topic if its label matches the new message above this.
const TOPIC_REUSE_THRESHOLD: f32 = 0.70;
/// Number of recent messages whose embeddings are averaged for topic baseline.
const ROLLING_WINDOW: usize = 5;

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of a topic check.
#[derive(Debug, Clone, PartialEq)]
pub enum TopicSwitchResult {
    /// Same topic — continue with full recall.
    SameTopic,
    /// Partial drift — use FactsOnly recall to reduce noise.
    PartialDrift,
    /// Full topic switch — new (or reused) topic_id assigned.
    Switched { new_topic_id: String },
}

impl TopicSwitchResult {
    /// Convert to the appropriate RecallScope for this result.
    pub fn to_recall_scope(&self) -> RecallScope {
        match self {
            TopicSwitchResult::SameTopic => RecallScope::Full,
            TopicSwitchResult::PartialDrift => RecallScope::FactsOnly,
            TopicSwitchResult::Switched { new_topic_id } => RecallScope::CurrentTopic {
                topic_id: new_topic_id.clone(),
            },
        }
    }
}

// ── TopicManager ──────────────────────────────────────────────────────────────

/// Manages topic state for a single agent session.
pub struct TopicManager {
    /// Current active topic ID.
    current_topic_id: Mutex<String>,
    /// Rolling average embedding of recent messages (empty = no history yet).
    /// Guarded by Mutex so TopicManager is Send+Sync despite Vec<f32>.
    rolling_embedding: Mutex<Vec<f32>>,
    /// Recent message texts for keyword-based fallback (last ROLLING_WINDOW).
    recent_messages: Mutex<Vec<String>>,
    /// Known topic labels (for reuse search without DB access).
    /// Maps topic_id → label.
    known_topics: Mutex<Vec<(String, String)>>,
}

impl TopicManager {
    /// Create a new `TopicManager` for a session, starting with a fresh topic.
    pub fn new() -> Self {
        Self {
            current_topic_id: Mutex::new(Uuid::new_v4().to_string()),
            rolling_embedding: Mutex::new(vec![]),
            recent_messages: Mutex::new(vec![]),
            known_topics: Mutex::new(vec![]),
        }
    }

    /// Create a `TopicManager` starting with an explicit `initial_topic_id`.
    pub fn with_topic(initial_topic_id: impl Into<String>) -> Self {
        Self {
            current_topic_id: Mutex::new(initial_topic_id.into()),
            rolling_embedding: Mutex::new(vec![]),
            recent_messages: Mutex::new(vec![]),
            known_topics: Mutex::new(vec![]),
        }
    }

    /// Get the current topic ID.
    pub fn current_topic_id(&self) -> String {
        self.current_topic_id.lock().unwrap().clone()
    }

    /// Check whether the new message belongs to the current topic.
    ///
    /// - When `embedder` is `Some`, uses cosine similarity of embeddings.
    /// - When `embedder` is `None`, falls back to keyword overlap.
    ///
    /// Updates internal state (rolling embedding, recent messages) and returns
    /// the `TopicSwitchResult`.
    pub async fn check_and_switch(
        &self,
        new_message: &str,
        embedder: Option<&dyn crate::embeddings::EmbeddingProvider>,
    ) -> Result<TopicSwitchResult> {
        // Update keyword history first (always, regardless of embedder)
        self.push_recent_message(new_message);

        if let Some(emb) = embedder {
            if emb.dim() > 0 {
                return self.check_with_embedding(new_message, emb).await;
            }
        }

        // Fallback: keyword-based comparison
        Ok(self.check_with_keywords(new_message))
    }

    // ── Embedding-based check ─────────────────────────────────────────────────

    async fn check_with_embedding(
        &self,
        new_message: &str,
        embedder: &dyn crate::embeddings::EmbeddingProvider,
    ) -> Result<TopicSwitchResult> {
        let new_emb = embedder.embed_one(new_message).await?;

        let rolling = self.rolling_embedding.lock().unwrap().clone();

        if rolling.is_empty() {
            // No baseline yet — this IS the baseline
            self.update_rolling_embedding(&new_emb);
            return Ok(TopicSwitchResult::SameTopic);
        }

        let sim = cosine_similarity(&rolling, &new_emb);
        self.update_rolling_embedding(&new_emb);

        if sim >= SAME_TOPIC_THRESHOLD {
            Ok(TopicSwitchResult::SameTopic)
        } else if sim >= RELATED_TOPIC_THRESHOLD {
            Ok(TopicSwitchResult::PartialDrift)
        } else {
            let new_topic = self.switch_to_new_topic_embedding(&new_emb)?;
            Ok(TopicSwitchResult::Switched { new_topic_id: new_topic })
        }
    }

    /// Try to reuse an existing topic by embedding similarity; create new if none match.
    fn switch_to_new_topic_embedding(&self, _new_emb: &[f32]) -> Result<String> {
        // Simple reuse: check keyword overlap of recent messages with known topic labels.
        // (Full vector search against topics table is done in the engine layer via SqliteMemory.)
        let new_topic_id = self.find_or_create_topic_by_keywords();
        *self.current_topic_id.lock().unwrap() = new_topic_id.clone();
        // Reset rolling embedding for the new topic
        *self.rolling_embedding.lock().unwrap() = vec![];
        Ok(new_topic_id)
    }

    fn update_rolling_embedding(&self, new_emb: &[f32]) {
        let mut rolling = self.rolling_embedding.lock().unwrap();
        if rolling.is_empty() {
            *rolling = new_emb.to_vec();
            return;
        }
        // Exponential moving average: new = 0.7 * old + 0.3 * new
        let alpha = 0.30_f32;
        for (r, n) in rolling.iter_mut().zip(new_emb.iter()) {
            *r = *r * (1.0 - alpha) + n * alpha;
        }
        // Re-normalise to unit length
        let norm: f32 = rolling.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-8 {
            rolling.iter_mut().for_each(|x| *x /= norm);
        }
    }

    // ── Keyword-based check ───────────────────────────────────────────────────

    fn check_with_keywords(&self, new_message: &str) -> TopicSwitchResult {
        let recent = self.recent_messages.lock().unwrap();
        if recent.len() < 2 {
            return TopicSwitchResult::SameTopic;
        }

        // Build word set from recent messages (excluding the newest = current message)
        let history_words: HashSet<String> = recent[..recent.len() - 1]
            .iter()
            .flat_map(|m| tokenize_simple(m))
            .collect();

        if history_words.is_empty() {
            return TopicSwitchResult::SameTopic;
        }

        let new_words: HashSet<String> = tokenize_simple(new_message).collect();
        let overlap = intersection_ratio(&history_words, &new_words);

        if overlap >= SAME_TOPIC_THRESHOLD {
            TopicSwitchResult::SameTopic
        } else if overlap >= RELATED_TOPIC_THRESHOLD {
            TopicSwitchResult::PartialDrift
        } else {
            let new_topic = self.find_or_create_topic_by_keywords();
            *self.current_topic_id.lock().unwrap() = new_topic.clone();
            TopicSwitchResult::Switched { new_topic_id: new_topic }
        }
    }

    // ── Topic reuse ───────────────────────────────────────────────────────────

    /// Search known topics for a label similar to the current messages.
    /// Creates a new UUID topic if no match found above `TOPIC_REUSE_THRESHOLD`.
    fn find_or_create_topic_by_keywords(&self) -> String {
        let recent = self.recent_messages.lock().unwrap();
        let new_words: HashSet<String> = recent
            .iter()
            .flat_map(|m| tokenize_simple(m))
            .collect();

        let known = self.known_topics.lock().unwrap();
        let best = known
            .iter()
            .map(|(id, label)| {
                let label_words: HashSet<String> = tokenize_simple(label).collect();
                let score = intersection_ratio(&new_words, &label_words);
                (id, score)
            })
            .filter(|(_, score)| *score >= TOPIC_REUSE_THRESHOLD)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((existing_id, _)) = best {
            existing_id.clone()
        } else {
            let new_id = Uuid::new_v4().to_string();
            // We can't add to known_topics here because we hold the lock.
            // The engine layer is responsible for registering the new topic.
            new_id
        }
    }

    /// Register a topic label for future reuse matching.
    pub fn register_topic(&self, topic_id: impl Into<String>, label: impl Into<String>) {
        let mut known = self.known_topics.lock().unwrap();
        let id = topic_id.into();
        // Update if exists, else push
        if let Some(entry) = known.iter_mut().find(|(tid, _)| tid == &id) {
            entry.1 = label.into();
        } else {
            known.push((id, label.into()));
        }
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    fn push_recent_message(&self, msg: &str) {
        let mut recent = self.recent_messages.lock().unwrap();
        recent.push(msg.to_string());
        if recent.len() > ROLLING_WINDOW {
            recent.remove(0);
        }
    }
}

impl Default for TopicManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Math helpers ──────────────────────────────────────────────────────────────

/// Cosine similarity between two equal-length vectors.
/// Returns 0.0 if either vector is zero-length or mismatched.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a < 1e-8 || norm_b < 1e-8 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Jaccard-like intersection ratio: |A∩B| / |A|  (one-sided, relative to A).
fn intersection_ratio(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() {
        return 0.0;
    }
    let common = a.intersection(b).count();
    common as f32 / a.len() as f32
}

/// Tokenise a message into lowercase words, filtering stop words and short tokens.
fn tokenize_simple(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
}

/// Common stop words (English + Chinese connectives).
static STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "shall", "should",
    "may", "might", "must", "can", "could", "to", "of", "in", "on", "at", "by",
    "for", "with", "about", "as", "from", "that", "this", "it", "its",
    "and", "or", "but", "not", "no", "so", "if", "then", "than", "when",
    "my", "your", "his", "her", "our", "their", "we", "you", "they", "he", "she",
    "me", "him", "us", "them", "what", "which", "who", "how",
    "我", "你", "他", "她", "它", "我们", "你们", "他们", "这", "那", "是", "的",
    "了", "在", "和", "也", "都", "就", "不", "很", "会", "要", "有",
];

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0_f32, 0.0, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_intersection_ratio_full_overlap() {
        let a: HashSet<String> = ["rust", "code"].iter().map(|s| s.to_string()).collect();
        let b = a.clone();
        assert!((intersection_ratio(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_intersection_ratio_no_overlap() {
        let a: HashSet<String> = ["rust"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["poem"].iter().map(|s| s.to_string()).collect();
        assert_eq!(intersection_ratio(&a, &b), 0.0);
    }

    #[test]
    fn test_keyword_same_topic() {
        let tm = TopicManager::new();
        // Prime with several Rust-related messages
        for msg in &["help me with rust traits", "rust ownership model", "rust async await"] {
            tm.push_recent_message(msg);
        }
        let result = tm.check_with_keywords("how do lifetimes work in rust");
        // "rust" appears in both history and new → should be same or partial
        assert!(
            result == TopicSwitchResult::SameTopic || result == TopicSwitchResult::PartialDrift,
            "Expected same/partial topic, got {:?}", result
        );
    }

    #[test]
    fn test_keyword_topic_switch() {
        let tm = TopicManager::new();
        // Prime with Rust messages
        for msg in &["rust traits", "rust borrow checker", "rust lifetime"] {
            tm.push_recent_message(msg);
        }
        // Completely unrelated new message
        let result = tm.check_with_keywords("write me a haiku about autumn leaves falling");
        assert_eq!(result, TopicSwitchResult::Switched {
            new_topic_id: match result.clone() {
                TopicSwitchResult::Switched { new_topic_id } => new_topic_id,
                _ => panic!("Expected Switched"),
            }
        });
    }

    #[test]
    fn test_topic_switch_result_to_recall_scope() {
        assert_eq!(TopicSwitchResult::SameTopic.to_recall_scope(), RecallScope::Full);
        assert_eq!(TopicSwitchResult::PartialDrift.to_recall_scope(), RecallScope::FactsOnly);

        let sw = TopicSwitchResult::Switched { new_topic_id: "t1".to_string() };
        assert_eq!(sw.to_recall_scope(), RecallScope::CurrentTopic { topic_id: "t1".to_string() });
    }

    #[test]
    fn test_new_topic_manager_has_nonempty_topic_id() {
        let tm = TopicManager::new();
        assert!(!tm.current_topic_id().is_empty());
    }

    #[test]
    fn test_register_and_reuse_topic() {
        let tm = TopicManager::new();
        let first_id = tm.current_topic_id();
        tm.register_topic(first_id.clone(), "rust programming");

        // Prime with messages that match the registered topic label
        for msg in &["rust programming discussion"] {
            tm.push_recent_message(msg);
        }

        let reused = tm.find_or_create_topic_by_keywords();
        assert_eq!(reused, first_id, "Should reuse the registered rust topic");
    }
}
