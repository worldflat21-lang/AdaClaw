//! Groq Provider
//!
//! Groq 提供 OpenAI-compatible LLM 接口 + Whisper 语音转录。
//!
//! ## LLM 端点
//! `https://api.groq.com/openai/v1/chat/completions`（OpenAI-compatible）
//!
//! ## Whisper 转录
//! `POST https://api.groq.com/openai/v1/audio/transcriptions`
//! - model: `whisper-large-v3`
//! - 支持 voice/audio 文件字节（multipart/form-data）

use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::registry::ProviderSpec;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

const BASE_URL: &str = "https://api.groq.com/openai/v1";
const WHISPER_MODEL: &str = "whisper-large-v3";

// ── GroqProvider ──────────────────────────────────────────────────────────────

pub struct GroqProvider {
    key: Option<String>,
    base_url: String,
    client: Client,
}

impl GroqProvider {
    pub fn new(key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            key: key.map(|s| s.to_string()),
            base_url: url.unwrap_or(BASE_URL).trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
        let mut msgs = Vec::new();
        if let Some(sys) = req.system {
            msgs.push(serde_json::json!({"role": "system", "content": sys}));
        }
        for m in req.messages {
            msgs.push(serde_json::json!({"role": m.role, "content": m.content}));
        }
        msgs
    }

    fn auth_header(&self) -> Option<String> {
        self.key.as_ref().map(|k| format!("Bearer {}", k))
    }
}

#[async_trait]
impl Provider for GroqProvider {
    fn name(&self) -> &str {
        "groq"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false, // Groq LLM 当前不支持 vision
            streaming: true,
        }
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        let messages = Self::build_messages(&req);
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": temp,
        });

        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);

        if let Some(auth) = self.auth_header() {
            builder = builder.header("Authorization", auth);
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Groq API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        let content = data["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        debug!(model = %model, chars = content.len(), "Groq response");
        Ok(ChatResponse { content })
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: msg.to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system,
        };
        Ok(self.chat(req, model, temp).await?.content)
    }
}

// ── Whisper 语音转录 ───────────────────────────────────────────────────────────

/// Groq Whisper 转录器
///
/// 将音频字节转录为文本。
/// 用于 Telegram voice/audio 消息的自动转录。
pub struct GroqWhisper {
    key: String,
    base_url: String,
    client: Client,
}

impl GroqWhisper {
    pub fn new(key: impl Into<String>, base_url: Option<&str>) -> Self {
        Self {
            key: key.into(),
            base_url: base_url.unwrap_or(BASE_URL).trim_end_matches('/').to_string(),
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
            .header("Authorization", format!("Bearer {}", self.key))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Groq Whisper API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        let text = data["text"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        debug!(chars = text.len(), "Whisper transcription complete");
        Ok(text)
    }
}

// ── ProviderSpec ──────────────────────────────────────────────────────────────

pub const SPEC: ProviderSpec = ProviderSpec {
    name: "groq",
    aliases: &["groq-llama", "llama-3"],
    local: false,
    capabilities: ProviderCapabilities {
        native_tool_calling: true,
        vision: false,
        streaming: true,
    },
    factory: |key, url| Box::new(GroqProvider::new(key, url)),
};
