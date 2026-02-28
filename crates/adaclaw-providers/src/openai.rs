use adaclaw_core::provider::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use anyhow::Result;
use async_trait::async_trait;
use crate::error::ProviderError;
use crate::registry::ProviderSpec;
use reqwest::Client;
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    key: Option<String>,
    base_url: String,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            key: key.map(|s| s.to_string()),
            base_url: url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
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
    name: "openai",
    aliases: &["gpt-4", "gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"],
    local: false,
    capabilities: ProviderCapabilities {
        native_tool_calling: true,
        vision: true,
        streaming: true,
    },
    factory: |key, url| Box::new(OpenAiProvider::new(key, url)),
};
