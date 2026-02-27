/// Query Memory Decomposition (QMD)
///
/// Complex user queries often contain multiple distinct concepts that a single
/// vector/keyword search cannot capture simultaneously.  QMD breaks a query
/// into focused sub-queries, runs them in parallel against the memory backend,
/// then re-ranks the combined results with Reciprocal Rank Fusion (RRF).
///
/// ## Example
///
/// User asks: *"上周我让你处理的那个报告，用的是什么方法？"*
///
/// QMD decomposes this into:
///   1. `"上周"`
///   2. `"报告 处理"`
///   3. `"处理方法"`
///
/// Each sub-query is run independently; results are merged via RRF so that
/// entries relevant to **multiple** sub-queries bubble to the top.
///
/// ## Fallback
///
/// If the LLM decomposition call fails (network error, API quota, etc.) QMD
/// transparently falls back to a plain `memory.recall(original_query)`.
use adaclaw_core::memory::{Memory, MemoryEntry, RecallScope};
use adaclaw_core::provider::Provider;
use anyhow::Result;
use futures_util::future::join_all;

/// Decompose `query` into focused sub-queries using an LLM, then recall from
/// `memory` using each sub-query in parallel and merge results via RRF.
///
/// # Parameters
/// - `memory`   — any `Memory` impl
/// - `provider` — LLM used for decomposition (uses a cheap call at temp=0.0)
/// - `model`    — model name for the decomposition LLM call
/// - `query`    — the original user query
/// - `limit`    — maximum number of entries to return
/// - `session`  — optional session filter passed to each recall
/// - `scope`    — recall scope (topic isolation / clean mode)
///
/// # Fallback
/// On any error during decomposition or sub-queries, returns a plain
/// `memory.recall(query, limit, session, scope)` result.
pub async fn recall_with_qmd(
    memory: &dyn Memory,
    provider: &dyn Provider,
    model: &str,
    query: &str,
    limit: usize,
    session: Option<&str>,
    scope: RecallScope,
) -> Result<Vec<MemoryEntry>> {
    // Step 1: Ask the LLM to decompose the query into sub-queries.
    let sub_queries = match decompose_query(provider, model, query).await {
        Ok(qs) if !qs.is_empty() => qs,
        Ok(_) => {
            tracing::debug!(query, "QMD returned empty decomposition, using original query");
            vec![query.to_string()]
        }
        Err(e) => {
            tracing::warn!(query, error = %e, "QMD decomposition failed, falling back to plain recall");
            return memory.recall(query, limit, session, scope).await;
        }
    };

    tracing::debug!(
        query,
        sub_queries = ?sub_queries,
        "QMD decomposed query"
    );

    // Step 2: Run each sub-query recall concurrently.
    let fetch_n = (limit * 2).max(10);
    let session_owned = session.map(|s| s.to_string());

    let futures: Vec<_> = sub_queries
        .iter()
        .map(|sq| {
            let sq = sq.clone();
            let sess = session_owned.clone();
            let sc = scope.clone();
            async move {
                memory
                    .recall(&sq, fetch_n, sess.as_deref(), sc)
                    .await
                    .unwrap_or_default()
            }
        })
        .collect();

    let all_results: Vec<Vec<MemoryEntry>> = join_all(futures).await;

    // Step 3: Flatten into per-sub-query key lists and merge with RRF.
    if all_results.is_empty() || all_results.iter().all(|r| r.is_empty()) {
        return memory.recall(query, limit, session, scope).await;
    }

    let ranked_lists: Vec<Vec<String>> = all_results
        .iter()
        .map(|entries| entries.iter().map(|e| e.key.clone()).collect())
        .collect();

    let merged_keys: Vec<String> = if ranked_lists.len() == 1 {
        ranked_lists[0].clone()
    } else {
        merge_multiple_ranked_lists(&ranked_lists)
    };

    let top_keys: Vec<String> = merged_keys.into_iter().take(limit).collect();

    let mut entry_map: std::collections::HashMap<String, MemoryEntry> = all_results
        .into_iter()
        .flatten()
        .map(|e| (e.key.clone(), e))
        .collect();

    let ordered: Vec<MemoryEntry> = top_keys
        .into_iter()
        .filter_map(|k| entry_map.remove(&k))
        .collect();

    if ordered.is_empty() {
        memory.recall(query, limit, session, scope).await
    } else {
        Ok(ordered)
    }
}

// ── Decomposition ─────────────────────────────────────────────────────────────

/// Ask the LLM to decompose `query` into 2–4 focused sub-queries.
///
/// Returns a list of sub-query strings.  The original query is always included
/// as the first element so that a plain single-query recall is still covered.
async fn decompose_query(
    provider: &dyn Provider,
    model: &str,
    query: &str,
) -> Result<Vec<String>> {
    let system = "You are a memory retrieval assistant. \
                  Your job is to decompose a complex query into 2-4 shorter, \
                  focused sub-queries that together cover all aspects of the original. \
                  Output ONLY a JSON array of strings, no explanation. \
                  Example: [\"sub-query 1\", \"sub-query 2\", \"sub-query 3\"]";

    let prompt = format!(
        "Decompose this query into 2-4 focused sub-queries for memory search:\n\n\"{}\"",
        query
    );

    let raw = provider
        .chat_with_system(Some(system), &prompt, model, 0.0)
        .await?;

    parse_sub_queries(&raw, query)
}

/// Parse the LLM JSON response into a Vec<String>.
/// Falls back gracefully if JSON parsing fails.
fn parse_sub_queries(raw: &str, original_query: &str) -> Result<Vec<String>> {
    // Try to find a JSON array in the response (model may wrap it in markdown fences)
    let trimmed = raw.trim();
    let json_str = if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        &trimmed[start..=end]
    } else {
        trimmed
    };

    let mut queries: Vec<String> = serde_json::from_str(json_str)
        .unwrap_or_else(|_| {
            // If JSON parse fails, treat each non-empty line as a sub-query
            trimmed
                .lines()
                .map(|l| l.trim().trim_matches(|c| c == '"' || c == '-' || c == '*').trim().to_string())
                .filter(|l| !l.is_empty() && l.len() > 2)
                .collect()
        });

    // Always include the original query to ensure base coverage
    if !queries.iter().any(|q| q == original_query) {
        queries.insert(0, original_query.to_string());
    }

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    queries.retain(|q| seen.insert(q.clone()));

    // Cap at 5 sub-queries to prevent excessive LLM calls
    queries.truncate(5);

    Ok(queries)
}

// ── Multi-list RRF ────────────────────────────────────────────────────────────

/// Merge N ranked lists into one using pairwise RRF folding.
fn merge_multiple_ranked_lists(lists: &[Vec<String>]) -> Vec<String> {
    assert!(!lists.is_empty());
    if lists.len() == 1 {
        return lists[0].clone();
    }

    // Accumulate RRF scores across all lists
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for list in lists {
        for (rank, key) in list.iter().enumerate() {
            *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (60.0 + rank as f64 + 1.0);
        }
    }

    let mut results: Vec<(String, f64)> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(k, _)| k).collect()
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sub_queries_clean_json() {
        let raw = r#"["上周", "报告处理", "处理方法"]"#;
        let qs = parse_sub_queries(raw, "original").unwrap();
        assert!(qs.contains(&"original".to_string()));
        assert!(qs.contains(&"上周".to_string()));
        assert!(qs.contains(&"报告处理".to_string()));
    }

    #[test]
    fn test_parse_sub_queries_fenced_json() {
        let raw = "```json\n[\"query A\", \"query B\"]\n```";
        let qs = parse_sub_queries(raw, "original").unwrap();
        assert!(qs.contains(&"query A".to_string()));
    }

    #[test]
    fn test_parse_sub_queries_original_included() {
        // Original must always be included even if not in LLM response
        let raw = r#"["sub1", "sub2"]"#;
        let qs = parse_sub_queries(raw, "my original query").unwrap();
        assert!(qs.contains(&"my original query".to_string()));
    }

    #[test]
    fn test_parse_sub_queries_dedup() {
        let raw = r#"["same", "same", "different"]"#;
        let qs = parse_sub_queries(raw, "original").unwrap();
        let count = qs.iter().filter(|q| q.as_str() == "same").count();
        assert_eq!(count, 1, "duplicates should be removed");
    }

    #[test]
    fn test_merge_multiple_ranked_lists_boosts_common() {
        let lists = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string(), "a".to_string()],
            vec!["a".to_string(), "d".to_string()],
        ];
        let merged = merge_multiple_ranked_lists(&lists);
        assert_eq!(merged[0], "a", "'a' appears in all lists and should rank first");
    }

    #[test]
    fn test_merge_single_list_passthrough() {
        let lists = vec![vec!["x".to_string(), "y".to_string()]];
        let merged = merge_multiple_ranked_lists(&lists);
        assert_eq!(merged, vec!["x", "y"]);
    }
}
