/// Memory Consolidation — periodic memory refresh and de-duplication.
///
/// ## Problem
///
/// Over time a memory store accumulates noise:
///
/// - Multiple entries that describe the *same* fact in slightly different ways
///   (e.g., the agent stored "user prefers Python" and later "user likes Python")
/// - Outdated entries that conflict with newer ones
/// - Conversation index entries (`Category::Conversation`) that become stale
///
/// A single-pass "forget the old ones" TTL solves the staleness problem, but
/// it can't detect semantic redundancy.
///
/// ## Solution
///
/// `consolidate()` runs a two-phase pass:
///
/// **Phase 1 — Cluster similar entries.**
/// Entries in a given category are loaded and sent to an LLM in small batches.
/// The LLM returns a JSON list of merge groups: sets of keys whose content is
/// semantically redundant.
///
/// **Phase 2 — Merge each cluster.**
/// For each merge group, the LLM produces a single consolidated entry that
/// preserves all unique facts.  The old keys are deleted and the merged entry
/// is stored under the key of the most recent member.
///
/// ## Usage
///
/// ```rust
/// use adaclaw_memory::consolidation::consolidate;
///
/// // Run weekly from the cron scheduler
/// consolidate(&memory, &provider, "gpt-4o-mini", &[Category::Core, Category::Daily], 50).await?;
/// ```
///
/// ## Safety
///
/// - Consolidation is **non-destructive by default**: if the LLM call fails,
///   the original entries are left untouched.
/// - The function processes at most `batch_size` entries per category to avoid
///   overwhelming the context window.
/// - `Category::Global` entries are intentionally **excluded** from automatic
///   consolidation (they should only be managed manually).
use adaclaw_core::memory::{Category, Memory, MemoryEntry};
use adaclaw_core::provider::Provider;
use anyhow::Result;
use tracing::{debug, info, warn};

/// Minimum number of entries in a category before consolidation is attempted.
const MIN_ENTRIES_FOR_CONSOLIDATION: usize = 5;

/// Maximum entries passed to the LLM in one clustering call (context limit).
const DEFAULT_BATCH_SIZE: usize = 50;

// ── Public API ────────────────────────────────────────────────────────────────

/// Run memory consolidation for the given categories.
///
/// # Parameters
/// - `memory`      — backend to read/write/delete entries
/// - `provider`    — LLM provider for clustering and merging
/// - `model`       — model name (a small/cheap model is fine)
/// - `categories`  — list of categories to consolidate (never touches `Global`)
/// - `batch_size`  — max entries per LLM clustering call (default: 50)
///
/// Returns the total number of entries deleted (originals replaced by merges).
pub async fn consolidate(
    memory: &dyn Memory,
    provider: &dyn Provider,
    model: &str,
    categories: &[Category],
    batch_size: Option<usize>,
) -> Result<usize> {
    let batch = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);
    let mut total_deleted = 0usize;

    for category in categories {
        // Never consolidate Global — those are manually curated
        if category == &Category::Global {
            continue;
        }

        let entries = memory.list(Some(category), None).await?;

        if entries.len() < MIN_ENTRIES_FOR_CONSOLIDATION {
            debug!(
                category = ?category,
                count = entries.len(),
                "Skipping consolidation (too few entries)"
            );
            continue;
        }

        // Process in batches to stay within context limits
        for chunk in entries.chunks(batch) {
            match consolidate_batch(memory, provider, model, chunk).await {
                Ok(deleted) => {
                    total_deleted += deleted;
                }
                Err(e) => {
                    warn!(
                        category = ?category,
                        error = %e,
                        "Consolidation batch failed, skipping (originals preserved)"
                    );
                }
            }
        }
    }

    info!(total_deleted, "Memory consolidation complete");
    Ok(total_deleted)
}

// ── Internal ──────────────────────────────────────────────────────────────────

/// Process one batch of entries: cluster semantically similar ones, merge each
/// cluster, delete originals, store merged entry.
async fn consolidate_batch(
    memory: &dyn Memory,
    provider: &dyn Provider,
    model: &str,
    entries: &[MemoryEntry],
) -> Result<usize> {
    // Step 1: Ask the LLM to identify clusters of similar entries.
    let clusters = find_similar_clusters(provider, model, entries).await?;

    if clusters.is_empty() {
        debug!("No similar clusters found in batch of {}", entries.len());
        return Ok(0);
    }

    let mut total_deleted = 0usize;

    for cluster_keys in &clusters {
        if cluster_keys.len() < 2 {
            continue; // Nothing to merge
        }

        // Gather the actual MemoryEntry objects for this cluster
        let cluster_entries: Vec<&MemoryEntry> = cluster_keys
            .iter()
            .filter_map(|k| entries.iter().find(|e| &e.key == k))
            .collect();

        if cluster_entries.len() < 2 {
            continue;
        }

        // Step 2: Merge the cluster into one entry
        match merge_cluster(provider, model, &cluster_entries).await {
            Ok(merged_content) => {
                // Use the first key as the canonical merged key
                let canonical_key = &cluster_entries[0].key;
                let category = cluster_entries[0].category.clone();

                // Store merged entry (no topic — consolidated entries are topic-agnostic)
                memory
                    .store(canonical_key, &merged_content, category, None, None)
                    .await?;

                // Delete all other keys in the cluster
                for entry in &cluster_entries[1..] {
                    if memory.forget(&entry.key).await.unwrap_or(false) {
                        total_deleted += 1;
                    }
                }

                debug!(
                    canonical_key,
                    merged_count = cluster_entries.len(),
                    "Merged cluster into single entry"
                );
            }
            Err(e) => {
                warn!(
                    keys = ?cluster_keys,
                    error = %e,
                    "Merge failed for cluster, skipping"
                );
            }
        }
    }

    Ok(total_deleted)
}

/// Ask the LLM to identify groups of semantically redundant entries.
///
/// Returns a list of clusters, each cluster being a list of keys that should
/// be merged.  Only clusters with ≥2 members are meaningful.
async fn find_similar_clusters(
    provider: &dyn Provider,
    model: &str,
    entries: &[MemoryEntry],
) -> Result<Vec<Vec<String>>> {
    let system = "You are a memory deduplication assistant. \
                  Analyze the memory entries and identify groups of entries \
                  that contain redundant or highly overlapping information. \
                  Return a JSON array of arrays, where each inner array contains \
                  the keys of entries that should be merged. \
                  Only include groups with 2 or more entries. \
                  If no duplicates exist, return an empty array []. \
                  Example: [[\"key1\", \"key3\"], [\"key2\", \"key5\", \"key7\"]]";

    let entries_text: String = entries
        .iter()
        .map(|e| format!("KEY: {}\nCONTENT: {}", e.key, truncate(&e.content, 200)))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    let prompt = format!(
        "Identify groups of semantically redundant memory entries:\n\n{}",
        entries_text
    );

    let raw = provider
        .chat_with_system(Some(system), &prompt, model, 0.0)
        .await?;

    parse_clusters_json(&raw)
}

/// Ask the LLM to produce a single merged entry from a cluster.
async fn merge_cluster(
    provider: &dyn Provider,
    model: &str,
    entries: &[&MemoryEntry],
) -> Result<String> {
    let system = "You are a memory consolidation assistant. \
                  Merge the following related memory entries into a single, \
                  comprehensive entry that preserves all unique facts. \
                  Be concise — remove repetition while keeping everything important. \
                  Return only the merged content, no explanation.";

    let entries_text: String = entries
        .iter()
        .map(|e| format!("- {}", e.content))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Merge these related memory entries into one:\n\n{}",
        entries_text
    );

    provider
        .chat_with_system(Some(system), &prompt, model, 0.2)
        .await
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_clusters_json(raw: &str) -> Result<Vec<Vec<String>>> {
    let trimmed = raw.trim();
    // Extract JSON array from potential markdown fences
    let json_str = if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        &trimmed[start..=end]
    } else {
        return Ok(vec![]); // No array found → no clusters
    };

    let parsed: Vec<Vec<String>> = serde_json::from_str(json_str).unwrap_or_default();
    // Filter out singleton clusters
    Ok(parsed.into_iter().filter(|g| g.len() >= 2).collect())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut idx = max;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_clusters_empty() {
        let clusters = parse_clusters_json("[]").unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_parse_clusters_valid() {
        let raw = r#"[["key1", "key2"], ["key3", "key4", "key5"]]"#;
        let clusters = parse_clusters_json(raw).unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].len(), 2);
        assert_eq!(clusters[1].len(), 3);
    }

    #[test]
    fn test_parse_clusters_filters_singletons() {
        let raw = r#"[["key1"], ["key2", "key3"]]"#;
        let clusters = parse_clusters_json(raw).unwrap();
        // Singleton ["key1"] should be filtered out
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0], vec!["key2", "key3"]);
    }

    #[test]
    fn test_parse_clusters_with_fenced_json() {
        let raw = "```json\n[[\"a\", \"b\"]]\n```";
        let clusters = parse_clusters_json(raw).unwrap();
        assert_eq!(clusters.len(), 1);
    }

    #[test]
    fn test_parse_clusters_malformed() {
        // Malformed JSON → return empty (non-fatal)
        let clusters = parse_clusters_json("this is not json").unwrap();
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_long() {
        let s = "a".repeat(300);
        assert_eq!(truncate(&s, 200).len(), 200);
    }
}
