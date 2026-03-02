use crate::error::ProviderError;
use crate::registry::ProviderSpec;
use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::Result;
use async_trait::async_trait;
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
            base_url: url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
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
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::from_status(
                status, &text, None,
            )));
        }

        let data: Value = resp.json().await?;
        // Ollama response: { "message": { "role": "assistant", "content": "..." } }
        let content = data["message"]["content"]
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

    async fn warmup(&self) -> Result<()> {
        // Check if Ollama is reachable
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(anyhow::anyhow!("Ollama warmup failed: HTTP {}", r.status())),
            Err(e) => Err(anyhow::anyhow!(
                "Ollama not reachable at {}: {}",
                self.base_url,
                e
            )),
        }
    }

    /// Discover locally installed Ollama models via `GET /api/tags`.
    ///
    /// Returns model names in the format `name:tag` (e.g. `"llama3:latest"`),
    /// which is exactly what Ollama expects as the `model` field.
    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        let resp = match self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None), // Ollama not running — not an error
        };

        if !resp.status().is_success() {
            return Ok(None);
        }

        let data: Value = resp.json().await?;
        // Ollama /api/tags: { "models": [{ "name": "llama3:latest", ... }] }
        let models: Vec<String> = data["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if models.is_empty() {
            Ok(None)
        } else {
            Ok(Some(models))
        }
    }
}

pub fn spec() -> ProviderSpec {
    ProviderSpec {
        name: "ollama",
        // Note: "mistral" and "qwen" aliases removed — those names now route
        // to dedicated cloud providers.  Use `base_url = "http://localhost:11434"`
        // in the config to run any model locally through Ollama instead.
        aliases: &["ollama", "llama3", "llama3.2", "deepseek-r1"],
        local: true,
        capabilities: ProviderCapabilities {
            native_tool_calling: false,
            vision: false,
            streaming: true,
        },
        factory: Box::new(|key, url| Box::new(OllamaProvider::new(key, url))),
    }
}
