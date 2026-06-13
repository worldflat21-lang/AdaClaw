pub mod error;
pub mod openai_compat;
pub mod openai_proto;
pub mod registry;
pub mod reliable;
pub mod router;

// Standalone providers (require custom HTTP logic beyond OpenAI-compat)
pub mod anthropic;
pub mod ollama;
pub mod openai;

// Groq's LLM endpoint is OpenAI-compatible (served by `openai_compat`); this
// module retains only the Whisper speech-transcription helper, which has no
// OpenAI-compatible equivalent.
pub mod groq;
