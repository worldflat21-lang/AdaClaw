//! Structured error types for the `adaclaw-channels` library crate.
//!
//! Using [`thiserror`] allows callers to `match` on specific error variants
//! rather than inspecting opaque [`anyhow::Error`] strings.
//!
//! ## Pattern
//!
//! Channel implementations return `Result<T, ChannelError>` internally.
//! The `Channel` trait methods wrap these in `anyhow::Error` via `?` so the
//! public trait API continues to return `anyhow::Result`.
//! Callers may `downcast_ref::<ChannelError>()` when specific handling is needed.

use thiserror::Error;

/// Errors produced by channel implementations in `adaclaw-channels`.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// HTTP request to external API (Telegram, Discord, etc.) failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Authentication or signature verification failed.
    #[error("Authentication error: {0}")]
    Auth(String),

    /// The channel connection was dropped / lost.
    #[error("Channel disconnected")]
    Disconnected,

    /// Configuration is missing or invalid.
    #[error("Configuration error: {0}")]
    Config(String),

    /// JSON serialisation / deserialisation error.
    #[error("Serialisation error: {0}")]
    Serde(#[from] serde_json::Error),

    /// I/O error (used by channels with local file/socket I/O).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Operation timed out.
    #[error("Operation timed out")]
    Timeout,

    /// WebSocket-level error (Discord, Slack Socket Mode).
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// The platform returned an unexpected or error response.
    #[error("Platform error: {0}")]
    Platform(String),
}
