/// EmbeddingProvider — abstracts over local (fastembed) and remote (OpenAI) embedding backends.
///
/// Implementations must be `Send + Sync` so they can be shared across async tasks.
use anyhow::Result;
use async_trait::async_trait;

pub mod none;

#[cfg(feature = "fastembed")]
pub mod fastembed;

#[cfg(feature = "openai-embed")]
pub mod openai;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Converts text into fixed-size embedding vectors.
///
/// All implementations must return vectors of exactly `self.dim()` dimensions.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Human-readable backend name (e.g. "fastembed", "openai", "none").
    fn name(&self) -> &str;

    /// Embedding vector size in number of `f32` elements.
    fn dim(&self) -> usize;

    /// Embed a batch of text strings.
    ///
    /// Returns `Ok(Vec<Vec<f32>>)` where `result[i].len() == self.dim()`.
    /// Implementations should batch efficiently.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single string — convenience wrapper around `embed`.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut batch = self.embed(&[text]).await?;
        batch.pop().ok_or_else(|| anyhow::anyhow!("Embedding returned empty result"))
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// Build an `EmbeddingProvider` from a backend name string.
///
/// - `"fastembed"` → local AllMiniLML6V2 (requires feature `fastembed`)
/// - `"openai"`    → OpenAI text-embedding-3-small (requires feature `openai-embed`)
/// - `"none"` / any unrecognised string → no-op (FTS5-only recall)
pub fn create_embedding_provider(
    backend: &str,
    _api_key: Option<&str>,
    _base_url: Option<&str>,
) -> Result<Box<dyn EmbeddingProvider>> {
    match backend {
        #[cfg(feature = "fastembed")]
        "fastembed" => {
            let p = fastembed::FastEmbedProvider::new()?;
            Ok(Box::new(p))
        }

        #[cfg(feature = "openai-embed")]
        "openai" => {
            let key = _api_key
                .map(ToString::to_string)
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("OpenAI embedding requires an API key"))?;
            let base = _base_url
                .map(ToString::to_string)
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            Ok(Box::new(openai::OpenAiEmbedProvider::new(key, base)))
        }

        // Graceful fallback: any unrecognised backend → NoopEmbedProvider
        _ => {
            if backend != "none" && !backend.is_empty() {
                tracing::warn!(
                    backend,
                    "Unknown embedding backend (or feature not compiled in), falling back to 'none'"
                );
            }
            Ok(Box::new(none::NoopEmbedProvider))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert a `Vec<f32>` to a raw bytes `Vec<u8>` (little-endian f32 blob).
/// This is the format expected by `sqlite-vec`.
pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect()
}

/// Convert raw bytes back to `Vec<f32>` (little-endian).
pub fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_roundtrip() {
        let original = vec![0.1_f32, 0.5, -0.3, 1.0];
        let bytes = vec_to_bytes(&original);
        let recovered = bytes_to_vec(&bytes);
        for (a, b) in original.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn test_noop_provider() {
        let p = create_embedding_provider("none", None, None).unwrap();
        assert_eq!(p.name(), "none");
        assert_eq!(p.dim(), 0);
    }
}
