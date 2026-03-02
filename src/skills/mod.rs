//! Skills system — load user-defined skill instructions from `workspace/skills/`.
//!
//! A skill is a directory containing a `SKILL.md` (or `SKILL.toml`) file that
//! describes what the agent can do. Skills are injected into the system prompt
//! so the LLM knows about available capabilities.
//!
//! ## Directory layout
//! ```text
//! workspace/
//!   skills/
//!     weather/
//!       SKILL.md     ← instructions for the weather skill
//!     github/
//!       SKILL.md
//!       SKILL.toml   ← optional structured metadata (name/description/version)
//! ```

pub mod loader;

pub use loader::{Skill, load_skills, skills_to_prompt};
