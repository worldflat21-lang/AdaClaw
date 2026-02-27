use anyhow::Result;
use async_trait::async_trait;

use super::EmbeddingProvider;

/// No-op embedding provider — always returns empty vectors.
///
/// When this provider is active, `SqliteMemory::recall()` gracefully degrades
/// to pure FTS5 keyword search (no vector component in RRF).
pub struct NoopEmbedProvider;

#[async_trait]
impl EmbeddingProvider for NoopEmbedProvider {
    fn name(&self) -> &str {
        "none"
    }

    fn dim(&self) -> usize {
        0
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![]; texts.len()])
    }
}
