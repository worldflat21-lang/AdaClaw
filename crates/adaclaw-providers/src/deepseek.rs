use crate::registry::ProviderSpec;
/// DeepSeek provider — OpenAI-compatible endpoint.
///
/// DeepSeek exposes an OpenAI-compatible API at `https://api.deepseek.com/v1`.
/// The two main models are:
///   • `deepseek-chat`   — fast, cheap, general-purpose (≈GPT-4o-mini quality)
///   • `deepseek-reasoner` — slow, cheap, chain-of-thought reasoning (≈o1 quality)
///
/// Because the API is fully OpenAI-compatible, this implementation is a thin
/// wrapper that fixes the base URL and provider name.
use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";

pub struct DeepSeekProvider {
    /// Phase 14-P1-2: API key wrapped in `Secret<String>`.
    key: Option<Secret<String>>,
    base_url: String,
    client: Client,
}

impl DeepSeekProvider {
    pub fn new(key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            key: key.map(|s| Secret::new(s.to_string())),
            base_url: url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
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
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn name(&self) -> &str {
        "deepseek"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false, // deepseek-chat v3 does not support vision yet
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

        if let Some(key) = &self.key {
            builder = builder.header("Authorization", format!("Bearer {}", key.expose_secret()));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("DeepSeek API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        let content = data["choices"][0]["message"]["content"]
            .as_str()
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

/// Legacy spec — functionality is now handled by `openai_compat::spec_for("deepseek")`.
#[allow(deprecated)]
pub fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "deepseek",
        aliases: &["deepseek-chat", "deepseek-reasoner", "ds"],
        local: false,
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        factory: Box::new(|key, url| Box::new(DeepSeekProvider::new(key, url))),
    }
}
