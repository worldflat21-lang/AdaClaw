pub mod error;
pub mod openai_compat;
pub mod registry;
pub mod reliable;
pub mod router;

// Standalone providers (require custom HTTP logic beyond OpenAI-compat)
pub mod anthropic;
pub mod ollama;
pub mod openai;

// Legacy per-provider modules — kept for reference; functionality is now
// handled by `openai_compat`.  Will be removed in a future cleanup pass.
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod deepseek;
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod groq;
#[deprecated(since = "0.1.0", note = "use openai_compat instead")]
#[allow(deprecated)]
pub mod openrouter;
