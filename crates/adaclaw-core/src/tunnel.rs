use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Tunnel: Send + Sync {
    fn name(&self) -> &str;
    async fn connect(&self) -> Result<String>;
    async fn disconnect(&self) -> Result<()>;
}
