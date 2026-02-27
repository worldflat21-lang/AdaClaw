use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ObserverEvent {
    Started,
    Stopped,
    Error(String),
}

#[async_trait]
pub trait Observer: Send + Sync {
    fn name(&self) -> &str;
    async fn observe(&self, event: ObserverEvent) -> Result<()>;
}
