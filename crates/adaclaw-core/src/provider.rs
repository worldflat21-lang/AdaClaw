use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub native_tool_calling: bool,
    pub vision: bool,
    pub streaming: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub system: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    /// Chain-of-thought reasoning text returned by thinking/reasoning models
    /// (DeepSeek-R1, Kimi K2.5, QwQ, etc.).  `None` for standard models.
    ///
    /// Sourced from either:
    ///   • the `reasoning_content` field in the API response, or
    ///   • text inside `<think>…</think>` tags stripped from `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn supports_native_tools(&self) -> bool {
        self.capabilities().native_tool_calling
    }
    fn supports_vision(&self) -> bool {
        self.capabilities().vision
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse>;
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String>;
    async fn warmup(&self) -> Result<()> {
        Ok(())
    }

    /// Dynamically discover models available from this provider.
    ///
    /// Calls `GET {base_url}/v1/models` (OpenAI-compatible) and returns the
    /// list of model IDs.  Providers that do not support model listing should
    /// keep the default `Ok(None)` implementation.
    ///
    /// This mirrors the "dynamic model discovery" feature found in other
    /// agents (Moltis), useful for Ollama (locally installed models vary per
    /// machine) and OpenRouter (hundreds of models, impossible to enumerate
    /// statically).
    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}
