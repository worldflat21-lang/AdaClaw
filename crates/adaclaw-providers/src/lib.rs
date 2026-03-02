pub mod error;
pub mod openai_compat;
pub mod registry;
pub mod router;
pub mod reliable;

// Standalone providers (require custom HTTP logic beyond OpenAI-compat)
pub mod openai;
pub mod anthropic;
pub mod ollama;

// Legacy per-provider modules — kept for reference; functionality is now
// handled by `openai_compat`.  Will be removed in a future cleanup pass.
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod deepseek;
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod openrouter;
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod groq;
