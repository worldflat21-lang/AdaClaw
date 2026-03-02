//! MCP 客户端实现（Model Context Protocol）
//!
//! 把外部 MCP Server 暴露的工具**透明包装为 `Tool` trait 实现**，
//! 与原生工具（shell/file/http）完全同等对待。
//!
//! ## 配置示例（config.toml，与 Claude Desktop / nanobot 格式兼容）
//!
//! ```toml
//! [tools.mcp_servers.filesystem]
//! command = "npx"
//! args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
//!
//! [tools.mcp_servers.my-remote-mcp]
//! url     = "https://example.com/mcp/"
//! headers = { Authorization = "Bearer xxxxx" }
//! tool_timeout = 30
//! ```

pub mod http;
pub mod loader;
pub mod stdio;

use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

// ── JSON-RPC 2.0 公共类型 ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub method: String,
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: i64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub id: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

// ── MCP Protocol 初始化 ───────────────────────────────────────────────────────

pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// MCP tool 描述（来自 tools/list 响应）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolDescription {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

/// MCP transport 抽象——支持 stdio 和 http 两种实现
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 调用 tools/call，返回 ToolResult
    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult>;
    /// 返回 transport 名称（用于日志）
    fn transport_name(&self) -> &str;
}

// ── McpTool ───────────────────────────────────────────────────────────────────

/// 将单个 MCP Server 工具包装为 `Tool` trait，与原生工具完全等同
pub struct McpTool {
    /// MCP Server 配置名称（如 "filesystem"）
    pub server_name: String,
    /// MCP Server 中的工具名（如 "read_file"）
    pub tool_name: String,
    description: String,
    schema: Value,
    transport: Arc<dyn McpTransport>,
    /// 工具调用超时（秒，默认 30）
    timeout_secs: u64,
}

impl Clone for McpTool {
    fn clone(&self) -> Self {
        Self {
            server_name: self.server_name.clone(),
            tool_name: self.tool_name.clone(),
            description: self.description.clone(),
            schema: self.schema.clone(),
            transport: Arc::clone(&self.transport),
            timeout_secs: self.timeout_secs,
        }
    }
}

impl McpTool {
    pub fn new(
        server_name: impl Into<String>,
        desc: &McpToolDescription,
        transport: Arc<dyn McpTransport>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            tool_name: desc.name.clone(),
            description: desc
                .description
                .clone()
                .unwrap_or_else(|| desc.name.clone()),
            schema: desc.input_schema.clone().unwrap_or(serde_json::json!({
                "type": "object",
                "properties": {}
            })),
            transport,
            timeout_secs,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.tool_name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let transport = Arc::clone(&self.transport);
        let tool_name = self.tool_name.clone();
        let timeout = self.timeout_secs;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            transport.call_tool(&tool_name, args),
        )
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "MCP tool '{}' timed out after {}s",
                    tool_name, timeout
                )),
            }),
        }
    }
}

// ── 工具调用结果解析 ───────────────────────────────────────────────────────────

/// 从 MCP tools/call 响应中提取文本输出
pub fn extract_tool_output(response: Value) -> ToolResult {
    // MCP spec：result.content 是 [ { type: "text", text: "..." }, ... ]
    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        let mut parts: Vec<String> = Vec::new();
        let mut has_error = false;

        for item in content {
            if item.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = item.get("text").and_then(|t| t.as_str())
            {
                parts.push(text.to_string());
            }
        }

        // Check isError flag
        if response
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            has_error = true;
        }

        let output = parts.join("\n");
        return ToolResult {
            success: !has_error,
            output: if has_error {
                String::new()
            } else {
                output.clone()
            },
            error: if has_error { Some(output) } else { None },
        };
    }

    // Fallback: stringify whole response
    ToolResult {
        success: true,
        output: serde_json::to_string_pretty(&response).unwrap_or_default(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tool_output_text() {
        let resp = serde_json::json!({
            "content": [{"type": "text", "text": "hello world"}]
        });
        let result = extract_tool_output(resp);
        assert!(result.success);
        assert_eq!(result.output, "hello world");
    }

    #[test]
    fn test_extract_tool_output_error() {
        let resp = serde_json::json!({
            "content": [{"type": "text", "text": "file not found"}],
            "isError": true
        });
        let result = extract_tool_output(resp);
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
