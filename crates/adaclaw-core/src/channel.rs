use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait MessageBus: Send + Sync {
    async fn send_inbound(&self, msg: InboundMessage) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Image(Vec<u8>),
    Audio(Vec<u8>),
    File { name: String, data: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: Uuid,
    pub channel: String,
    pub session_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: MessageContent,
    pub reply_to: Option<Uuid>,
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: Uuid,
    pub target_channel: String,
    pub target_session_id: String,
    pub content: MessageContent,
    pub reply_to: Option<Uuid>,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    /// 默认返回 false；实现类通过 BaseChannel::is_running() 覆盖
    fn is_running(&self) -> bool {
        false
    }

    /// Send an approval prompt with Approve/Deny interactive UI to a session.
    ///
    /// Channels that support interactive approval (e.g. Telegram inline keyboard)
    /// should override this method. The default implementation is a no-op.
    ///
    /// # Arguments
    /// - `session_id` — target chat/channel ID to send the prompt to
    /// - `request_id` — pending approval request ID (e.g. `"apr-abc12"`)
    /// - `tool_name` — name of the tool awaiting approval
    /// - `args_preview` — short preview of the tool's arguments (≤200 chars)
    async fn send_approval_prompt(
        &self,
        session_id: &str,
        request_id: &str,
        tool_name: &str,
        args_preview: &str,
    ) -> Result<()> {
        let _ = (session_id, request_id, tool_name, args_preview);
        Ok(())
    }

    /// Whether this channel supports interactive approval prompts (e.g. inline buttons).
    ///
    /// Returns `true` for channels that implement `send_approval_prompt()` meaningfully.
    fn supports_approval_prompts(&self) -> bool {
        false
    }
}
