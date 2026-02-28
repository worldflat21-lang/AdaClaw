//! MCP Stdio Transport
//!
//! 启动本地进程（npx / uvx / 可执行文件），通过 stdin/stdout 进行 JSON-RPC 通信。
//! 进程崩溃时自动重启（最多 3 次）。
//!
//! ## MCP JSON-RPC 协议
//!
//! 消息以换行符分隔（newline-delimited JSON，NDJSON）：
//! - 客户端 → 服务器：写入 stdin
//! - 服务器 → 客户端：读取 stdout
//!
//! 初始化顺序：
//! 1. `initialize`（握手）
//! 2. `notifications/initialized`（确认）
//! 3. `tools/list`（获取工具清单）

use super::{
    extract_tool_output, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    McpToolDescription, McpTransport, MCP_PROTOCOL_VERSION,
};
use adaclaw_core::tool::ToolResult;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const MAX_RESTARTS: u32 = 3;

// ── Process 状态 ──────────────────────────────────────────────────────────────

struct ProcessState {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    restart_count: u32,
}

// ── StdioMcpClient ────────────────────────────────────────────────────────────

/// MCP Stdio transport：通过 stdin/stdout 与本地进程通信
pub struct StdioMcpClient {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    state: Mutex<Option<ProcessState>>,
    next_id: AtomicI64,
    server_name: String,
}

impl StdioMcpClient {
    /// 创建并初始化 Stdio MCP 客户端（立即启动进程并握手）
    pub async fn connect(
        server_name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
    ) -> Result<(Arc<Self>, Vec<McpToolDescription>)> {
        let server_name = server_name.into();
        let command = command.into();
        let env = env.unwrap_or_default();

        let client = Arc::new(Self {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            state: Mutex::new(None),
            next_id: AtomicI64::new(1),
            server_name: server_name.clone(),
        });

        // 启动进程并握手
        let mut state_guard = client.state.lock().await;
        let ps = client.spawn_process(&command, &args, &env).await?;
        *state_guard = Some(ps);
        drop(state_guard);

        // 握手：initialize → notifications/initialized
        client.do_initialize().await?;

        // 获取工具列表
        let tools = client.do_list_tools().await?;

        Ok((client, tools))
    }

    /// 启动子进程
    async fn spawn_process(
        &self,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<ProcessState> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()); // 转发 stderr 便于调试

        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {} {:?}", command, args))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin from MCP process"))?;
        let raw_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout from MCP process"))?;
        let stdout = BufReader::new(raw_stdout);

        Ok(ProcessState {
            child,
            stdin,
            stdout,
            restart_count: 0,
        })
    }

    /// 重启进程（最多 MAX_RESTARTS 次）
    async fn restart_if_needed(&self, state: &mut ProcessState) -> Result<()> {
        if state.restart_count >= MAX_RESTARTS {
            return Err(anyhow!(
                "MCP server '{}' crashed {} times, giving up",
                self.server_name,
                MAX_RESTARTS
            ));
        }
        warn!(
            server = %self.server_name,
            restart_count = state.restart_count + 1,
            "MCP server process died, restarting..."
        );
        let _ = state.child.kill().await;
        let mut new_state = self
            .spawn_process(&self.command, &self.args, &self.env)
            .await?;
        new_state.restart_count = state.restart_count + 1;
        // 重新握手
        *state = new_state;
        self.do_initialize_with_state(state).await?;
        Ok(())
    }

    /// 发送 JSON-RPC 请求并等待响应（持有 state guard）
    async fn send_request_with_state(
        &self,
        state: &mut ProcessState,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);
        let line = serde_json::to_string(&req)? + "\n";

        state
            .stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("Failed to write to MCP server stdin ({})", method))?;
        state.stdin.flush().await?;

        debug!(server = %self.server_name, method = %method, id = id, "→ MCP request");

        // 读取响应行（跳过服务器发来的通知消息）
        loop {
            let mut response_line = String::new();
            state
                .stdout
                .read_line(&mut response_line)
                .await
                .with_context(|| "Failed to read from MCP server stdout")?;

            if response_line.is_empty() {
                return Err(anyhow!("MCP server '{}' closed stdout", self.server_name));
            }

            let trimmed = response_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resp: serde_json::Value = serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse MCP response: {}", trimmed))?;

            // 跳过 notifications（无 id 字段或 id 为 null）
            if resp.get("method").is_some() {
                debug!(server = %self.server_name, "← MCP notification (skipped)");
                continue;
            }

            let rpc_resp: JsonRpcResponse = serde_json::from_value(resp)?;

            if let Some(err) = rpc_resp.error {
                return Err(anyhow!(
                    "MCP server error ({}): {} (code {})",
                    method,
                    err.message,
                    err.code
                ));
            }

            debug!(server = %self.server_name, method = %method, "← MCP response");
            return Ok(rpc_resp.result.unwrap_or(Value::Null));
        }
    }

    /// 发送通知（无需等待响应）
    async fn send_notification_with_state(
        &self,
        state: &mut ProcessState,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&notif)? + "\n";
        state.stdin.write_all(line.as_bytes()).await?;
        state.stdin.flush().await?;
        Ok(())
    }

    /// initialize 握手（获取 state guard）
    async fn do_initialize(&self) -> Result<()> {
        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| anyhow!("MCP state not initialized"))?;
        self.do_initialize_with_state(state).await
    }

    /// initialize 握手（持有 state）
    async fn do_initialize_with_state(&self, state: &mut ProcessState) -> Result<()> {
        let params = serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "adaclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        self.send_request_with_state(state, "initialize", params)
            .await
            .with_context(|| {
                format!("MCP initialize failed for server '{}'", self.server_name)
            })?;

        // 确认初始化
        self.send_notification_with_state(
            state,
            "notifications/initialized",
            serde_json::json!({}),
        )
        .await?;

        Ok(())
    }

    /// 获取工具列表
    async fn do_list_tools(&self) -> Result<Vec<McpToolDescription>> {
        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| anyhow!("MCP state not initialized"))?;

        let result = self
            .send_request_with_state(state, "tools/list", serde_json::json!({}))
            .await?;

        let tools: Vec<McpToolDescription> =
            serde_json::from_value(result["tools"].clone()).unwrap_or_default();

        Ok(tools)
    }
}

#[async_trait]
impl McpTransport for StdioMcpClient {
    fn transport_name(&self) -> &str {
        &self.server_name
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": args
        });

        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| anyhow!("MCP state not initialized"))?;

        // 尝试发送请求
        let result = self
            .send_request_with_state(state, "tools/call", params.clone())
            .await;

        match result {
            Ok(resp) => Ok(extract_tool_output(resp)),
            Err(e) => {
                // 进程可能崩溃，尝试重启
                warn!(server = %self.server_name, tool = %name, error = %e, "MCP tool call failed");
                if let Err(restart_err) = self.restart_if_needed(state).await {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("MCP server failed and could not restart: {}", restart_err)),
                    });
                }
                // 重试一次
                match self.send_request_with_state(state, "tools/call", params).await {
                    Ok(resp) => Ok(extract_tool_output(resp)),
                    Err(e2) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("MCP tool '{}' failed after restart: {}", name, e2)),
                    }),
                }
            }
        }
    }
}
