/// OpenRouter provider — aggregates hundreds of models under a single API key.
///
/// API surface is identical to OpenAI's `/v1/chat/completions`, so this reuses
/// the same request/response format.  The only differences are:
///   • base URL: https://openrouter.ai/api/v1
///   • optional `HTTP-Referer` and `X-Title` headers for model-ranking on the
///     OpenRouter dashboard (configured via `extra` fields in ProviderConfig)
///
/// Model names follow the `provider/model` convention used by OpenRouter,
/// e.g. `openai/gpt-4o`, `anthropic/claude-3-5-sonnet`, `google/gemini-2-flash`.
/// Use `openrouter/auto` to let OpenRouter choose the best available model.
use adaclaw_core::provider::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::registry::ProviderSpec;
use reqwest::Client;
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    key: Option<String>,
    base_url: String,
    /// Optional site URL sent in `HTTP-Referer` header (for OpenRouter dashboard stats)
    site_url: Option<String>,
    /// Optional app name sent in `X-Title` header
    app_name: Option<String>,
    client: Client,
}

impl OpenRouterProvider {
    pub fn new(key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            key: key.map(|s| s.to_string()),
            base_url: url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
            site_url: None,
            app_name: Some("AdaClaw".to_string()),
            client: Client::new(),
        }
    }

    pub fn with_app_info(mut self, site_url: Option<String>, app_name: Option<String>) -> Self {
        self.site_url = site_url;
        self.app_name = app_name;
        self
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
impl Provider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // OpenRouter is a meta-aggregator; capabilities depend on the underlying model.
        // We report the superset of what most premium models support.
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
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
            builder = builder.header("Authorization", format!("Bearer {}", key));
        }

        // OpenRouter ranking / attribution headers (optional but recommended)
        if let Some(ref url) = self.site_url {
            builder = builder.header("HTTP-Referer", url);
        }
        if let Some(ref name) = self.app_name {
            builder = builder.header("X-Title", name);
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OpenRouter API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        let content = data["choices"][0]["message"]["content"]
            .as_str()
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
    name: "openrouter",
    aliases: &[
        "openrouter/auto",
        "or/auto",
        "or",
    ],
    local: false,
    capabilities: ProviderCapabilities {
        native_tool_calling: true,
        vision: true,
        streaming: true,
    },
    factory: |key, url| Box::new(OpenRouterProvider::new(key, url)),
};
