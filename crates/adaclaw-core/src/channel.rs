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
}
