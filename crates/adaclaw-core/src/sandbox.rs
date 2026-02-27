use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &str;
    async fn setup(&self) -> Result<()>;
    async fn teardown(&self) -> Result<()>;
}
