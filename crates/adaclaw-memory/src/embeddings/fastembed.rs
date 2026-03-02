/// FastEmbed embedding provider — local ONNX inference, zero API dependency.
///
/// Uses AllMiniLML6V2 (384-dimensional embeddings) from the fastembed crate.
/// The ONNX runtime is CPU-bound, so all inference is offloaded to
/// `tokio::task::spawn_blocking` to avoid stalling the async runtime.
use anyhow::Result;
use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::sync::{Arc, Mutex};

use super::EmbeddingProvider;

pub const DIM: usize = 384;

/// Thread-safe wrapper around `fastembed::TextEmbedding`.
///
/// `TextEmbedding` is `!Send` due to raw ONNX pointers, so we store it inside
/// `Arc<Mutex<...>>` and always call it from within `spawn_blocking`.
pub struct FastEmbedProvider {
    inner: Arc<Mutex<TextEmbedding>>,
}

impl FastEmbedProvider {
    /// Initialize the AllMiniLML6V2 model.
    ///
    /// The first call downloads / caches the ONNX model locally (~22 MB).
    /// Subsequent calls load from the cache instantly.
    pub fn new() -> Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(true),
        )?;
        Ok(Self {
            inner: Arc::new(Mutex::new(model)),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn name(&self) -> &str {
        "fastembed"
    }

    fn dim(&self) -> usize {
        DIM
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Clone the owned strings so we can move them into spawn_blocking.
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let inner = Arc::clone(&self.inner);

        tokio::task::spawn_blocking(move || {
            let mut model = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("FastEmbed mutex poisoned"))?;
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            let embeddings = model.embed(refs, None)?;
            Ok(embeddings)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NOTE: This test downloads ~22 MB on first run.
    /// Run with `cargo test --features fastembed -- --ignored` to skip in CI.
    #[tokio::test]
    #[ignore]
    async fn test_fastembed_basic() {
        let provider = FastEmbedProvider::new().unwrap();
        assert_eq!(provider.dim(), DIM);

        let embeddings = provider
            .embed(&["hello world", "how are you"])
            .await
            .unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), DIM);
        assert_eq!(embeddings[1].len(), DIM);

        // Sanity: embeddings should be L2-normalised (magnitude ≈ 1)
        let mag: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (mag - 1.0).abs() < 0.01,
            "embedding not normalised: mag={}",
            mag
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_fastembed_similar() {
        let provider = FastEmbedProvider::new().unwrap();
        let embs = provider
            .embed(&["deployment decision", "how we decided to deploy"])
            .await
            .unwrap();

        // Cosine similarity should be > 0.5 for semantically similar phrases
        let dot: f32 = embs[0].iter().zip(embs[1].iter()).map(|(a, b)| a * b).sum();
        assert!(dot > 0.5, "Expected high similarity, got dot={}", dot);
    }
}
