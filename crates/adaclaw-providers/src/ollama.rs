use adaclaw_core::provider::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::registry::ProviderSpec;
use reqwest::Client;
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

pub struct OllamaProvider {
    base_url: String,
    client: Client,
}

impl OllamaProvider {
    pub fn new(_key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            base_url: url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
        let mut msgs = Vec::new();
        // Ollama supports system role in messages
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
impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: false,
            streaming: true,
        }
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        let messages = Self::build_messages(&req);
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "options": {
                "temperature": temp,
            },
        });

        let resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama API error {}: {}", status, text));
        }

        let data: Value = resp.json().await?;
        // Ollama response: { "message": { "role": "assistant", "content": "..." } }
        let content = data["message"]["content"]
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

    async fn warmup(&self) -> Result<()> {
        // Check if Ollama is reachable
        let resp = self.client.get(format!("{}/api/tags", self.base_url)).send().await;
        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(anyhow!("Ollama warmup failed: HTTP {}", r.status())),
            Err(e) => Err(anyhow!("Ollama not reachable at {}: {}", self.base_url, e)),
        }
    }
}

pub const SPEC: ProviderSpec = ProviderSpec {
    name: "ollama",
    aliases: &["llama3", "mistral", "qwen", "deepseek-r1"],
    local: true,
    capabilities: ProviderCapabilities {
        native_tool_calling: false,
        vision: false,
        streaming: true,
    },
    factory: |key, url| Box::new(OllamaProvider::new(key, url)),
};
