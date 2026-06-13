//! Groq Whisper speech transcription.
//!
//! Groq's LLM endpoint is OpenAI-compatible and is served by
//! [`crate::openai_compat`] (`spec_for("groq")`); the duplicate `GroqProvider`
//! that used to live here was removed.  What remains is the Whisper transcriber,
//! which has no OpenAI-compatible equivalent in our table.
//!
//! ## Whisper transcription
//! `POST https://api.groq.com/openai/v1/audio/transcriptions`
//! - model: `whisper-large-v3`
//! - accepts voice/audio file bytes (multipart/form-data)

use anyhow::{Result, anyhow};
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;
use tracing::debug;

const BASE_URL: &str = "https://api.groq.com/openai/v1";
const WHISPER_MODEL: &str = "whisper-large-v3";

// ── Whisper 语音转录 ───────────────────────────────────────────────────────────

/// Groq Whisper 转录器
///
/// 将音频字节转录为文本。
/// 用于 Telegram voice/audio 消息的自动转录。
pub struct GroqWhisper {
    /// Phase 14-P1-2: Whisper API key wrapped in `Secret<String>`.
    key: Secret<String>,
    base_url: String,
    client: Client,
}

impl GroqWhisper {
    pub fn new(key: impl Into<String>, base_url: Option<&str>) -> Self {
        Self {
            key: Secret::new(key.into()),
            base_url: base_url
                .unwrap_or(BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            client: Client::new(),
        }
    }

    /// 转录音频文件字节
    ///
    /// `audio_bytes`：音频文件内容（ogg/mp3/m4a/wav 等）
    /// `filename`：提示文件格式（如 "voice.ogg"）
    /// `language`：可选语言代码（如 "zh"，None = 自动检测）
    pub async fn transcribe(
        &self,
        audio_bytes: Vec<u8>,
        filename: &str,
        language: Option<&str>,
    ) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name(filename.to_string())
            .mime_str("audio/ogg")?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", WHISPER_MODEL)
            .part("file", part);

        if let Some(lang) = language {
            form = form.text("language", lang.to_string());
        }

        let resp = self
            .client
            .post(format!("{}/audio/transcriptions", self.base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.key.expose_secret()),
            )
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Groq Whisper API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        let text = data["text"].as_str().unwrap_or("").trim().to_string();

        debug!(chars = text.len(), "Whisper transcription complete");
        Ok(text)
    }
}
