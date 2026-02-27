use adaclaw_core::provider::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::registry::ProviderSpec;
use reqwest::Client;
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicProvider {
    key: Option<String>,
    base_url: String,
    client: Client,
}

impl AnthropicProvider {
    pub fn new(key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            key: key.map(|s| s.to_string()),
            base_url: url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
            client: Client::new(),
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
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": messages,
            "temperature": temp,
        });

        // system prompt goes at the top level
        if let Some(sys) = req.system {
            body["system"] = Value::String(sys.to_string());
        }

        let key = self
            .key
            .as_deref()
            .ok_or_else(|| anyhow!("Anthropic API key not set"))?;

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        // Anthropic response: { "content": [{ "type": "text", "text": "..." }] }
        let content = data["content"]
            .as_array()
            .and_then(|arr| arr.iter().find(|c| c["type"] == "text"))
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string();

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

pub const SPEC: ProviderSpec = ProviderSpec {
    name: "anthropic",
    aliases: &["claude", "claude-3", "claude-3-5-sonnet", "claude-sonnet-4"],
    local: false,
    capabilities: ProviderCapabilities {
        native_tool_calling: true,
        vision: true,
        streaming: true,
    },
    factory: |key, url| Box::new(AnthropicProvider::new(key, url)),
};
