//! Reciprocal Rank Fusion (RRF)
//!
//! Merges multiple ranked lists into a single ranked list.
//! Formula: score(d) = Σ 1/(k + rank(d))  where k is a smoothing constant (default 60).
//!
//! Reference: Cormack, G.V. et al. (2009). "Reciprocal Rank Fusion outperforms Condorcet
//! and individual rank learning methods." SIGIR 2009.

/// A single result item with its merged RRF score.
#[derive(Debug, Clone)]
pub struct RrfResult {
    /// The entry key (matches `MemoryEntry::key`)
    pub key: String,
    /// Combined RRF score (higher = more relevant)
    pub score: f64,
}

/// Merge two ranked key-lists using Reciprocal Rank Fusion.
///
/// `vec_ranked`  — keys sorted by ascending vector distance (most similar first)
/// `fts_ranked`  — keys sorted by descending BM25 rank (best match first)
/// `k`           — smoothing constant (60.0 is standard; higher = less sensitive to top ranks)
///
/// Returns results sorted by descending RRF score (best first).
pub fn reciprocal_rank_fusion(
    vec_ranked: &[String],
    fts_ranked: &[String],
    k: f64,
) -> Vec<RrfResult> {
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    for (rank, key) in vec_ranked.iter().enumerate() {
        *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
    }

    for (rank, key) in fts_ranked.iter().enumerate() {
        *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
    }

    let mut results: Vec<RrfResult> = scores
        .into_iter()
        .map(|(key, score)| RrfResult { key, score })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Convenience: merge with default k=60 (recommended by original paper).
pub fn rrf_merge(vec_ranked: &[String], fts_ranked: &[String]) -> Vec<RrfResult> {
    reciprocal_rank_fusion(vec_ranked, fts_ranked, 60.0)
}

// ── unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rrf_pure_vec() {
        let vec_ranked = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let fts_ranked = vec![];
        let results = rrf_merge(&vec_ranked, &fts_ranked);
        // All scores come only from vec; "a" should be first
        assert_eq!(results[0].key, "a");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_rrf_pure_fts() {
        let vec_ranked = vec![];
        let fts_ranked = vec!["x".to_string(), "y".to_string()];
        let results = rrf_merge(&vec_ranked, &fts_ranked);
        assert_eq!(results[0].key, "x");
    }

    #[test]
    fn test_rrf_fusion_boosts_common() {
        // "a" appears in both → gets highest score
        let vec_ranked = vec!["a".to_string(), "b".to_string()];
        let fts_ranked = vec!["c".to_string(), "a".to_string()];
        let results = rrf_merge(&vec_ranked, &fts_ranked);
        assert_eq!(results[0].key, "a", "common item should win");
    }

    #[test]
    fn test_rrf_disjoint_sets() {
        // No overlap — both lists contribute unique items
        let vec_ranked = vec!["v1".to_string(), "v2".to_string()];
        let fts_ranked = vec!["f1".to_string(), "f2".to_string()];
        let results = rrf_merge(&vec_ranked, &fts_ranked);
        assert_eq!(results.len(), 4);
        // First items of each list tie at rank 0 → same score
        assert!((results[0].score - results[1].score).abs() < 1e-12);
    }

    #[test]
    fn test_rrf_score_formula() {
        let vec_ranked = vec!["only".to_string()];
        let fts_ranked = vec!["only".to_string()];
        let results = rrf_merge(&vec_ranked, &fts_ranked);
        // score = 1/(60+1) * 2
        let expected = 2.0 / 61.0;
        assert!((results[0].score - expected).abs() < 1e-12);
    }

    #[test]
    fn test_rrf_custom_k() {
        let vec_ranked = vec!["a".to_string()];
        let fts_ranked = vec!["a".to_string()];
        let results = reciprocal_rank_fusion(&vec_ranked, &fts_ranked, 10.0);
        let expected = 2.0 / 11.0;
        assert!((results[0].score - expected).abs() < 1e-12);
    }
}
