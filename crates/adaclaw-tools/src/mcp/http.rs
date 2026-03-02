//! MCP HTTP Transport
//!
//! 通过 HTTP POST 与远程 MCP Server 通信（JSON-RPC over HTTP）。
//! 支持自定义 headers（Authorization 等），可配置超时。

use super::{
    JsonRpcRequest, JsonRpcResponse, MCP_PROTOCOL_VERSION, McpToolDescription, McpTransport,
    extract_tool_output,
};
use adaclaw_core::tool::ToolResult;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tracing::debug;

// ── HttpMcpClient ─────────────────────────────────────────────────────────────

/// MCP HTTP transport：通过 HTTP POST 向远程 MCP Server 发送 JSON-RPC 请求
pub struct HttpMcpClient {
    url: String,
    client: Client,
    next_id: AtomicI64,
    server_name: String,
}

impl HttpMcpClient {
    /// 创建并初始化 HTTP MCP 客户端
    ///
    /// 会执行 `initialize` + `tools/list` 握手，返回工具清单。
    pub async fn connect(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: Option<HashMap<String, String>>,
        timeout_secs: Option<u64>,
    ) -> Result<(Arc<Self>, Vec<McpToolDescription>)> {
        let server_name = server_name.into();
        let url = url.into().trim_end_matches('/').to_string() + "/";
        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(30));

        // 构建 HeaderMap
        let mut header_map = HeaderMap::new();
        if let Some(headers) = headers {
            for (k, v) in headers {
                if let (Ok(name), Ok(val)) = (HeaderName::from_str(&k), HeaderValue::from_str(&v)) {
                    header_map.insert(name, val);
                }
            }
        }

        let client = Client::builder()
            .timeout(timeout)
            .default_headers(header_map)
            .build()
            .context("Failed to build HTTP client for MCP")?;

        let transport = Arc::new(Self {
            url: url.clone(),
            client,
            next_id: AtomicI64::new(1),
            server_name: server_name.clone(),
        });

        // 握手：initialize
        transport.do_initialize().await?;

        // 获取工具列表
        let tools = transport.do_list_tools().await?;

        Ok((transport, tools))
    }

    /// 发送 JSON-RPC 请求并返回结果
    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);

        debug!(
            server = %self.server_name,
            method = %method,
            id = id,
            "→ MCP HTTP request"
        );

        let resp = self
            .client
            .post(&self.url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("HTTP request to MCP server '{}' failed", self.server_name))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "MCP server '{}' returned HTTP {}: {}",
                self.server_name,
                status,
                body
            ));
        }

        let rpc_resp: JsonRpcResponse = resp.json().await.with_context(|| {
            format!(
                "Failed to parse JSON-RPC response from '{}'",
                self.server_name
            )
        })?;

        if let Some(err) = rpc_resp.error {
            return Err(anyhow!(
                "MCP server '{}' error ({}): {} (code {})",
                self.server_name,
                method,
                err.message,
                err.code
            ));
        }

        debug!(server = %self.server_name, method = %method, "← MCP HTTP response");
        Ok(rpc_resp.result.unwrap_or(Value::Null))
    }

    async fn do_initialize(&self) -> Result<()> {
        let params = serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "adaclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        self.send_request("initialize", params)
            .await
            .with_context(|| format!("MCP initialize failed for '{}'", self.server_name))?;
        Ok(())
    }

    async fn do_list_tools(&self) -> Result<Vec<McpToolDescription>> {
        let result = self
            .send_request("tools/list", serde_json::json!({}))
            .await?;
        let tools: Vec<McpToolDescription> =
            serde_json::from_value(result["tools"].clone()).unwrap_or_default();
        Ok(tools)
    }
}

#[async_trait]
impl McpTransport for HttpMcpClient {
    fn transport_name(&self) -> &str {
        &self.server_name
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": args
        });

        match self.send_request("tools/call", params).await {
            Ok(resp) => Ok(extract_tool_output(resp)),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("MCP HTTP tool '{}' failed: {}", name, e)),
            }),
        }
    }
}
