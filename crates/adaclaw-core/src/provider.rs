use crate::tool::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub native_tool_calling: bool,
    pub vision: bool,
    pub streaming: bool,
}

/// A single tool invocation requested by the model.
///
/// Used on both the request and response side of native tool calling:
///   * In a [`ChatResponse`], it carries a call the model wants executed.
///   * In a [`ChatMessage`] with `role == "assistant"`, it records a call the
///     model previously made, so the provider can faithfully reconstruct the
///     turn on the follow-up request (OpenAI requires the original `tool_calls`
///     to be echoed back alongside the matching `tool` result messages).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Provider-assigned id linking a call to its result. Empty for providers
    /// that don't surface ids (we synthesise one in that case).
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Token usage reported by the provider for a single completion.
///
/// `None` on a [`ChatResponse`] when the provider didn't return usage. This is
/// the foundation for token-budget-based context compaction (preferable to the
/// message-count heuristic the engine currently uses).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// An image attached to a (user) message, for vision-capable models.
///
/// Stored already base64-encoded with its MIME type so providers can embed it
/// directly (OpenAI `image_url` data URL / Anthropic `image` source block)
/// without re-encoding.  Not persisted to durable history — images are
/// per-turn context, not long-term memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageData {
    /// MIME type, e.g. `"image/png"` or `"image/jpeg"`.
    pub media_type: String,
    /// Base64-encoded image bytes (no `data:` prefix).
    pub data_base64: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Tool calls issued by this (assistant) message. Empty for ordinary turns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// When `Some`, this message is a tool *result* and the value is the id of
    /// the [`ToolCall`] it answers. `role` should be `"tool"` in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Images attached to this (user) message, for vision-capable providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageData>,
}

impl ChatMessage {
    /// An ordinary text message (no tool metadata).
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    /// A user turn carrying one or more images alongside its text.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images,
        }
    }

    /// An assistant turn that issued one or more tool calls.
    pub fn assistant_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_calls: calls,
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    /// A tool-result turn answering a previous [`ToolCall`].
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub system: Option<&'a str>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Native tool calls the model wants executed this turn. Empty when the model
    /// returned a plain text answer (or when using the text-parsing path).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Token usage for this completion, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
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

    /// Chat with native tool calling enabled.
    ///
    /// Providers whose API supports first-class tool calling override this to
    /// send `tools` in the request and return any `tool_calls` the model emits.
    /// The default implementation **ignores** `tools` and falls back to a plain
    /// [`Provider::chat`] — correct for providers without native support, where
    /// the engine drives tool calling through text parsing instead.
    async fn chat_with_tools(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatResponse> {
        let _ = tools;
        self.chat(req, model, temp).await
    }

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
