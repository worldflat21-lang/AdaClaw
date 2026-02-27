use adaclaw_core::memory::{Category, Memory, MemoryEntry, RecallScope};
use anyhow::Result;
use async_trait::async_trait;

pub struct NoneMemory {}

impl Default for NoneMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl NoneMemory {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Memory for NoneMemory {
    fn name(&self) -> &str {
        "none"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: Category,
        _session: Option<&str>,
        _topic: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session: Option<&str>,
        _scope: RecallScope,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }

    async fn get(&self, _key: &str) -> Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&Category>,
        _session: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }

    async fn forget(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }
}
