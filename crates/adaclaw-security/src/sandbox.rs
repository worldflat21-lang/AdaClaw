//! Security sandbox implementations.
//!
//! Submodules:
//! - `workspace`  — File-path isolation + symlink detection + blocklist
//! - `landlock`   — Linux Landlock LSM (kernel ≥ 5.13, no-op elsewhere)
//! - `docker`     — Container environment detection + Full-mode safety check

pub mod docker;
pub mod landlock;
pub mod workspace;

// Re-export the primary sandbox implementation for backwards compatibility
pub use workspace::WorkspaceSandbox;
