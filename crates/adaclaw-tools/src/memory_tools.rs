use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub struct MemoryStoreTool {}

impl Default for MemoryStoreTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStoreTool {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store memory"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["key", "content"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

pub struct MemoryRecallTool {}

impl Default for MemoryRecallTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRecallTool {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Recall memory"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}

pub struct MemoryForgetTool {}

impl Default for MemoryForgetTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryForgetTool {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Forget memory"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" }
            },
            "required": ["key"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        })
    }
}
