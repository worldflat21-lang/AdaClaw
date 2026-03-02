//! Agent identity system.
//!
//! Loads an `IDENTITY.md` file from the workspace directory and provides
//! helper methods to inject the identity into the agent's system prompt.
//!
//! ## Loading order
//! 1. `workspace_dir/IDENTITY.md` (user-customized)
//! 2. Built-in default (if file does not exist)

pub mod loader;

pub use loader::{Identity, load_identity};
