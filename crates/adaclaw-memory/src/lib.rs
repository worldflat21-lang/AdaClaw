pub mod consolidation;
pub mod embeddings;
/// Phase 14-P0-2: structured error types for library crate consumers.
pub mod error;
pub mod factory;
pub mod global;
pub mod markdown;
pub mod none;
pub mod query;
pub mod rrf;
/// Phase 14-P0-3: dedicated session store (conversation history ≠ long-term memory).
pub mod session_store;
pub mod sqlite;
pub mod topic;
