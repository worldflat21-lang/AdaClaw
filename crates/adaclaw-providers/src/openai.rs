use crate::error::ProviderError;
use crate::registry::ProviderSpec;
use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use adaclaw_core::tool::ToolSpec;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    /// Phase 14-P1-2: API key wrapped in `Secret<String>` to prevent
    /// accidental exposure in logs, panic output, or memory dumps.
    key: Option<Secret<String>>,
    base_url: String,
    client: Client,
}

impl OpenAiProvider {
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

    /// Shared request path for [`Provider::chat`] and
    /// [`Provider::chat_with_tools`].  Sends the OpenAI `tools` array when
    /// `tools` is non-empty and parses any `tool_calls` / `usage` back.
    async fn chat_inner(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatResponse> {
        let messages = crate::openai_proto::build_messages(&req);
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": temp,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(crate::openai_proto::build_tools(tools));
        }

        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);

        if let Some(key) = &self.key {
            builder = builder.header("Authorization", format!("Bearer {}", key.expose_secret()));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // 在消费 body 之前先提取 Retry-After 头（429 速率限制时 OpenAI 会返回此头）
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
        let message = &data["choices"][0]["message"];
        let content = message["content"].as_str().unwrap_or("").to_string();

        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls: crate::openai_proto::parse_tool_calls(message),
            usage: crate::openai_proto::parse_usage(&data),
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        }
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        self.chat_inner(req, &[], model, temp).await
    }

    async fn chat_with_tools(
        &self,
        req: ChatRequest<'_>,
        tools: &[ToolSpec],
        model: &str,
        temp: f64,
    ) -> Result<ChatResponse> {
        self.chat_inner(req, tools, model, temp).await
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let messages = vec![ChatMessage::new("user", msg)];
        let req = ChatRequest {
            messages: &messages,
            system,
        };
        Ok(self.chat(req, model, temp).await?.content)
    }
}

pub fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "openai",
        aliases: &["gpt-4", "gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"],
        local: false,
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        factory: Box::new(|key, url| Box::new(OpenAiProvider::new(key, url))),
    }
}
