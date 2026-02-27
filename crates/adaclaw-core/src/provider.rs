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
}
