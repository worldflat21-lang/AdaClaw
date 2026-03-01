//! Structured error types for the `adaclaw-tools` library crate.
//!
//! Using [`thiserror`] allows callers to `match` on specific error variants
//! rather than inspecting opaque [`anyhow::Error`] strings.
//!
//! ## Pattern
//!
//! Internal helpers return `Result<T, ToolError>`.  The public `Tool::execute`
//! method wraps errors via `?` so the `Tool` trait continues to return
//! `anyhow::Result`.  Callers may `downcast_ref::<ToolError>()` when needed.

use thiserror::Error;

/// Errors produced by built-in tools in `adaclaw-tools`.
#[derive(Debug, Error)]
pub enum ToolError {
    /// I/O operation failed (file read/write, shell execution, etc.).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Path traversal / workspace boundary violation detected by the sandbox.
    #[error("Sandbox violation: {0}")]
    SandboxViolation(String),

    /// Request / operation timed out.
    #[error("Operation timed out")]
    Timeout,

    /// SSRF protection blocked the request.
    #[error("SSRF blocked: {0}")]
    SsrfBlocked(String),

    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// MCP (Model Context Protocol) server or tool error.
    #[error("MCP error: {0}")]
    McpError(String),

    /// Argument missing or invalid.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// Any other error (used for ad-hoc wrapping).
    #[error("Tool error: {0}")]
    Other(String),
}
