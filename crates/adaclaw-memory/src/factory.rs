use adaclaw_core::memory::Memory;
use anyhow::Result;
use std::sync::Arc;

/// Configuration for the memory factory.
pub struct MemoryFactoryConfig<'a> {
    /// Backend type: `"sqlite"` | `"markdown"` | `"none"`
    pub backend: &'a str,
    /// File path / directory (meaning depends on backend):
    /// - sqlite   → path to `.db` file (default: `"memory.db"`)
    /// - markdown → directory path (default: `"memory/"`)
    /// - none     → ignored
    pub path: &'a str,
    /// Embedding provider: `"fastembed"` | `"openai"` | `"none"`
    pub embedding_provider: &'a str,
    /// API key for OpenAI embedding (ignored for other providers)
    pub embed_api_key: Option<&'a str>,
    /// Base URL override for embedding API (e.g. self-hosted proxy)
    pub embed_base_url: Option<&'a str>,
}

impl<'a> Default for MemoryFactoryConfig<'a> {
    fn default() -> Self {
        Self {
            backend: "sqlite",
            path: "memory.db",
            embedding_provider: "none",
            embed_api_key: None,
            embed_base_url: None,
        }
    }
}

/// Create a memory backend from configuration.
///
/// | backend      | embedding_provider | result                              |
/// |--------------|--------------------|-------------------------------------|
/// | `"sqlite"`   | `"fastembed"`      | SQLite + FTS5 + sqlite-vec + RRF    |
/// | `"sqlite"`   | `"openai"`         | SQLite + FTS5 + sqlite-vec + RRF    |
/// | `"sqlite"`   | `"none"`           | SQLite + FTS5 only (no vectors)     |
/// | `"markdown"` | (any)              | File-based Markdown memory          |
/// | `"none"`     | (any)              | No-op in-memory backend             |
pub fn create_memory_with_config(cfg: &MemoryFactoryConfig<'_>) -> Result<Box<dyn Memory>> {
    match cfg.backend {
        "sqlite" => {
            let effective_path = if cfg.path.is_empty() {
                "memory.db"
            } else {
                cfg.path
            };

            // Build embedding provider (returns NoopEmbedProvider on failure / "none")
            let embedder = crate::embeddings::create_embedding_provider(
                cfg.embedding_provider,
                cfg.embed_api_key,
                cfg.embed_base_url,
            )?;

            // Wrap in Arc only if it actually provides vectors
            let arc_embedder = if embedder.dim() > 0 {
                Some(Arc::from(embedder))
            } else {
                None
            };

            Ok(Box::new(crate::sqlite::SqliteMemory::open(
                effective_path,
                arc_embedder,
            )?))
        }

        "markdown" => {
            let effective_path = if cfg.path.is_empty() { "memory" } else { cfg.path };
            Ok(Box::new(crate::markdown::MarkdownMemory::new(effective_path)?))
        }

        "none" | "" => Ok(Box::new(crate::none::NoneMemory::new())),

        _ => Err(anyhow::anyhow!(
            "Unknown memory backend: '{}'. Valid options: sqlite, markdown, none",
            cfg.backend
        )),
    }
}

/// Simplified factory for the common SQLite case (no embeddings).
///
/// - `"sqlite"` → SQLite + FTS5 at the given `path` (default: `"memory.db"`)
/// - `"markdown"` → file-based Markdown memory at `path`
/// - `"none"` / `""` → in-memory no-op backend
pub fn create_memory(backend: &str, path: &str) -> Result<Box<dyn Memory>> {
    create_memory_with_config(&MemoryFactoryConfig {
        backend,
        path,
        ..Default::default()
    })
}
