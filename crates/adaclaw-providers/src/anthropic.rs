use crate::error::ProviderError;
use crate::registry::ProviderSpec;
use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Default `max_tokens` for Anthropic requests.
///
/// Raised from 4096 to 8192 — Claude 3.5 Sonnet and newer models support at
/// least 8192 output tokens.  This value is used when no `max_tokens` override
/// is provided via `ProviderConfig`.
///
/// Configurable per-provider via `config.toml`:
/// ```toml
/// [providers.anthropic]
/// max_tokens = 16384
/// ```
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct AnthropicProvider {
    /// Phase 14-P1-2: API key wrapped in `Secret<String>`.
    key: Option<Secret<String>>,
    base_url: String,
    client: Client,
    /// Maximum output tokens per request.
    /// Defaults to `DEFAULT_MAX_TOKENS`; overrideable via `ProviderConfig`.
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(key: Option<&str>, url: Option<&str>, max_tokens: Option<u32>) -> Self {
        Self {
            key: key.map(|s| Secret::new(s.to_string())),
            base_url: url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            client: Client::new(),
            max_tokens: max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        }
    }

    /// Anthropic API does NOT accept "system" role inside messages array.
    /// System prompt is a top-level field. Filter it out from messages.
    fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
        req.messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        }
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        let messages = Self::build_messages(&req);

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": self.max_tokens,
            "messages": messages,
            "temperature": temp,
        });

        // system prompt goes at the top level
        if let Some(sys) = req.system {
            body["system"] = Value::String(sys.to_string());
        }

        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Anthropic API key not set"))?;

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // 在消费 body 之前先提取 Retry-After 头
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::from_status(
                status,
                &text,
                retry_after,
            )));
        }

        let data: Value = resp.json().await?;
        // Anthropic response: { "content": [{ "type": "text", "text": "..." }] }
        let content = data["content"]
            .as_array()
            .and_then(|arr| arr.iter().find(|c| c["type"] == "text"))
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string();

        Ok(ChatResponse {
            content,
            reasoning_content: None,
        })
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

pub fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "anthropic",
        aliases: &["claude", "claude-3", "claude-3-5-sonnet", "claude-sonnet-4"],
        local: false,
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        factory: Box::new(|key, url| Box::new(AnthropicProvider::new(key, url, None))),
    }
}
