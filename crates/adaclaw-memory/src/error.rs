//! Structured error types for the `adaclaw-memory` library crate.
//!
//! Using [`thiserror`] allows callers to `match` on specific error variants
//! rather than inspecting opaque [`anyhow::Error`] strings.
//!
//! ## Pattern
//!
//! Internal functions return `Result<T, MemoryError>`.  The trait impl methods
//! wrap these in `anyhow::Error::new(e)` via `?` so the public `Memory` trait
//! continues to return `anyhow::Result`.  Callers that need to distinguish
//! error types can use `err.downcast_ref::<MemoryError>()`.
//!
//! This matches the existing `ProviderError` pattern in `adaclaw-providers`.

use thiserror::Error;

/// Errors produced by the `adaclaw-memory` crate.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// SQLite operation failed.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Connection pool error (r2d2).
    #[error("Connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    /// Embedding computation failed (non-fatal in most paths — logged and skipped).
    #[error("Embedding failed: {0}")]
    Embedding(String),

    /// A requested entry was not found (e.g. `get()` returns `None` and caller
    /// treats missing as an error rather than `Option`).
    #[error("Entry not found: {0}")]
    NotFound(String),

    /// File I/O error (used by `MarkdownMemory`).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialisation / deserialisation error.
    #[error("Serialisation error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Mutex was poisoned (should never happen in practice).
    #[error("Internal lock error: {0}")]
    Lock(String),
}
