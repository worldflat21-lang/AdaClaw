use anyhow::Result;
use async_trait::async_trait;

/// Speech-to-text transcription of audio bytes (e.g. a voice message).
///
/// Implemented by providers that offer audio transcription (currently Groq
/// Whisper). Lives in `adaclaw-core` so channels can hold a transcriber without
/// depending on the providers crate.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe `audio_bytes`. `filename` hints the format (e.g. `voice.ogg`);
    /// `language` is an optional ISO code (`"zh"`/`"en"`), `None` = auto-detect.
    async fn transcribe(
        &self,
        audio_bytes: Vec<u8>,
        filename: &str,
        language: Option<&str>,
    ) -> Result<String>;
}
