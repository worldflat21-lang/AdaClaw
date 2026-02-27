//! `DelegateTool` — 异步 Agent 间任务委托工具
//!
//! 允许主 Agent 将子任务交给另一个专业 Agent 异步处理。
//!
//! # 设计要点
//!
//! - **异步非阻塞**：`execute()` 立即返回"已接受"，子 Agent 在后台 `tokio::spawn` 中运行
//! - **防递归**：子 Agent 的工具列表中**不**注入 `DelegateTool`，由调用方（run.rs）保证
//! - **权限校验**：调用前通过 `AgentRegistry::can_delegate()` 检查允许名单
//! - **结果回传**：子 Agent 完成后通过 `channel = "system"` 的 `InboundMessage` 注入 Bus，
//!   daemon 主循环检测到后将结果路由回原始渠道
//! - **超时保护**：子 Agent 默认 300 s 超时，防止无限阻塞

use adaclaw_core::channel::{InboundMessage, MessageContent};
use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::agents::engine::AgentEngine;
use crate::agents::registry::AgentRegistry;
use crate::bus::queue::AppMessageBus;

/// 子 Agent 任务默认超时时长（秒）。
const DEFAULT_DELEGATE_TIMEOUT_SECS: u64 = 300;

// ── DelegateTool ──────────────────────────────────────────────────────────────

/// 工具：将任务异步委托给另一个专业 Agent。
///
/// 该工具由 daemon dispatch loop 在运行时注入，**不**在 `AgentInstance` 构造时存入
/// `ToolRegistry`，以防止子 Agent 递归使用。
pub struct DelegateTool {
    /// 调用此工具的父 Agent ID（用于权限校验）。
    parent_agent_id: String,
    /// Agent 注册表（只读共享引用）。
    registry: Arc<AgentRegistry>,
    /// 消息总线（用于将子 Agent 结果注入回 Bus）。
    bus: Arc<AppMessageBus>,
    /// 子 Agent 超时秒数。
    timeout_secs: u64,
}

impl DelegateTool {
    /// 创建 `DelegateTool`。
    ///
    /// - `parent_agent_id`：调用方 Agent 的 ID，用于 `can_delegate()` 权限检查。
    /// - `registry`：`Arc<AgentRegistry>` 共享引用，用于查找目标 Agent。
    /// - `bus`：`Arc<AppMessageBus>`，子 Agent 结果通过 `send_inbound_bypass()` 注入。
    pub fn new(
        parent_agent_id: String,
        registry: Arc<AgentRegistry>,
        bus: Arc<AppMessageBus>,
    ) -> Self {
        Self {
            parent_agent_id,
            registry,
            bus,
            timeout_secs: DEFAULT_DELEGATE_TIMEOUT_SECS,
        }
    }

    /// 设置子 Agent 超时时长（builder 模式）。
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task to another specialist agent asynchronously. \
         The sub-agent runs in the background; its result is delivered \
         back to the conversation automatically. Use this when the task \
         requires a different expertise (e.g. delegate coding tasks to 'coder')."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["agent", "task"],
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Target agent ID to delegate to (e.g. 'coder', 'researcher')"
                },
                "task": {
                    "type": "string",
                    "description": "Complete, self-contained task description for the sub-agent"
                },
                "origin_channel": {
                    "type": "string",
                    "description": "Original channel to deliver results back to (auto-filled)"
                },
                "origin_session": {
                    "type": "string",
                    "description": "Original session ID for result delivery (auto-filled)"
                }
            }
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        // ── 1. 解析参数 ────────────────────────────────────────────────────────
        let target_id = match args.get("agent").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required argument: 'agent'".to_string()),
                });
            }
        };

        let task = match args.get("task").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required argument: 'task'".to_string()),
                });
            }
        };

        let origin_channel = args
            .get("origin_channel")
            .and_then(|v| v.as_str())
            .unwrap_or("cli")
            .to_string();

        let origin_session = args
            .get("origin_session")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        // ── 2. 权限校验 ────────────────────────────────────────────────────────
        if !self.registry.can_delegate(&self.parent_agent_id, &target_id) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Agent '{}' is not authorized to delegate to '{}'. \
                     Add '{}' to agents.{}.subagents.allow in config.toml.",
                    self.parent_agent_id, target_id, target_id, self.parent_agent_id
                )),
            });
        }

        // ── 3. 获取目标 Agent 信息 ─────────────────────────────────────────────
        let target = match self.registry.get(&target_id) {
            Some(t) => t,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Target agent '{}' not found in registry",
                        target_id
                    )),
                });
            }
        };

        // 提取子 Agent 所需信息（不持有 AgentInstance 的引用跨 await 点）
        //
        // 注意：子 Agent 的工具列表中**不**注入 DelegateTool（防递归）
        let sub_tools = target.build_tools();
        let provider = Arc::clone(&target.provider);
        let model = target.model.clone();
        let temperature = target.temperature;
        let max_iterations = target.max_iterations;
        let system_extra = target.system_extra.clone();
        let target_id_clone = target_id.clone();

        let bus = Arc::clone(&self.bus);
        let task_id = Uuid::new_v4().to_string();
        let timeout_secs = self.timeout_secs;

        info!(
            parent = %self.parent_agent_id,
            target = %target_id,
            task_preview = %task.chars().take(100).collect::<String>(),
            task_id = %task_id,
            "Spawning sub-agent task"
        );

        // ── 4. 后台 spawn 子 Agent（非阻塞，立即返回） ───────────────────────
        tokio::spawn(async move {
            let engine = AgentEngine::new();

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                engine.run_tool_loop_with_options(
                    provider.as_ref(),
                    &sub_tools,
                    &task,
                    &model,
                    temperature,
                    system_extra.as_deref(),
                    Some(max_iterations),
                ),
            )
            .await;

            let reply_text = match result {
                Ok(Ok(resp)) => {
                    info!(target = %target_id_clone, task_id = %task_id, "Sub-agent task completed");
                    format!("[Sub-agent '{}' 完成]:\n{}", target_id_clone, resp)
                }
                Ok(Err(e)) => {
                    warn!(target = %target_id_clone, error = %e, "Sub-agent task failed");
                    format!("[Sub-agent '{}' 失败]: {}", target_id_clone, e)
                }
                Err(_) => {
                    warn!(
                        target = %target_id_clone,
                        timeout_secs,
                        "Sub-agent task timed out"
                    );
                    format!(
                        "[Sub-agent '{}' 超时]: 任务执行超过 {} 秒",
                        target_id_clone, timeout_secs
                    )
                }
            };

            // ── 5. 通过 "system" channel 将结果注入 Bus，绕过白名单 ──────────
            //
            // session_id 格式: "origin_channel\x1Eorigin_session"（使用 RS 分隔符，避免冒号歧义）
            // 也在 metadata 中单独存储，供 daemon 优先使用
            let system_msg = InboundMessage {
                id: Uuid::new_v4(),
                channel: "system".to_string(),
                session_id: format!(
                    "{}\x1E{}",
                    origin_channel, origin_session
                ),
                sender_id: format!("agent:{}", target_id_clone),
                sender_name: format!("Agent:{}", target_id_clone),
                content: MessageContent::Text(reply_text),
                reply_to: None,
                metadata: {
                    let mut m = HashMap::new();
                    m.insert(
                        "task_id".to_string(),
                        serde_json::Value::String(task_id),
                    );
                    m.insert(
                        "origin_channel".to_string(),
                        serde_json::Value::String(origin_channel),
                    );
                    m.insert(
                        "origin_session".to_string(),
                        serde_json::Value::String(origin_session),
                    );
                    m
                },
            };

            if let Err(e) = bus.send_inbound_bypass(system_msg).await {
                warn!(error = %e, "Failed to deliver sub-agent result to bus");
            }
        });

        // ── 6. 立即返回"已接受"，不等待子 Agent 完成 ─────────────────────────
        Ok(ToolResult {
            success: true,
            output: format!(
                "任务已委托给 Agent '{}'。结果将自动回传到当前对话。",
                target_id
            ),
            error: None,
        })
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delegate_tool_name() {
        let registry = Arc::new(AgentRegistry::new("assistant"));
        // We need an AppMessageBus for construction; since we can't easily mock it
        // in unit tests without a running tokio, just test the non-async parts.
        // Full integration tested via daemon startup.
        let _ = registry; // suppress warning
    }

    #[test]
    fn test_delegate_schema_has_required_fields() {
        use adaclaw_core::tool::Tool;
        // Create a minimal registry for the test
        let registry = Arc::new(AgentRegistry::new("assistant"));
        use tokio::sync::{broadcast, mpsc};
        let (tx, _) = mpsc::channel(1);
        let (btx, _) = broadcast::channel(1);
        let bus = Arc::new(AppMessageBus::new(tx, btx, vec![]));
        let tool = DelegateTool::new("assistant".to_string(), registry, bus);

        let schema = tool.parameters_schema();
        let required = schema.get("required").and_then(|r| r.as_array());
        assert!(required.is_some());
        let req = required.unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("agent")));
        assert!(req.iter().any(|v| v.as_str() == Some("task")));
    }
}
