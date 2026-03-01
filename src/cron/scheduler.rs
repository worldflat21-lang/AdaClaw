//! Cron 调度器 + Heartbeat 系统（Phase 10）
//!
//! ## Heartbeat
//!
//! 读取工作区根目录的 `HEARTBEAT.md`（或配置指定路径），
//! 提取 `- [ ] 任务描述` 行，按配置间隔触发一次 Agent 执行。
//!
//! 结果通过 MessageBus 回传到指定渠道（`target_channel`）。
//!
//! ## 动态渠道追踪（StateManager 集成）
//!
//! 当 `[heartbeat] target_channel` **未配置**时，Heartbeat 会从 `StateManager`
//! 读取最近活跃会话（`last_session_id`），将心跳结果发回给最近与 Agent 交互的用户。
//!
//! 这与 picoclaw 的 `state.GetLastChannel()` 行为对应。
//!
//! ## HEARTBEAT.md 格式
//!
//! ```markdown
//! # Heartbeat Tasks
//!
//! - [ ] 检查今日天气并发送早报
//! - [ ] 扫描未读邮件摘要
//! - [x] 已完成的任务（会被跳过）
//! ```

use crate::agents::engine::AgentEngine;
use crate::agents::message_tool::MessageTool;
use crate::agents::registry::AgentRegistry;
use crate::bus::queue::AppMessageBus;
use crate::config::schema::HeartbeatConfig;
use crate::state::StateManager;
use adaclaw_core::channel::{InboundMessage, MessageContent, MessageBus, OutboundMessage};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ── TaskKind / HeartbeatTask ──────────────────────────────────────────────────

/// How a heartbeat task should be executed.
///
/// Determined by the section header in `HEARTBEAT.md`:
///
/// | Section header               | Kind            |
/// |------------------------------|-----------------|
/// | `## Quick Tasks` (or none)   | `Quick`         |
/// | `## Long Tasks`              | `Spawn`         |
///
/// **Quick** — injected into the `MessageBus` and handled by the normal
/// agent dispatch loop.  The agent reply is sent back through the same channel.
/// Suitable for lightweight, sub-second tasks (greetings, status checks, etc.).
///
/// **Spawn** — the scheduler directly spawns a dedicated `AgentEngine` in a
/// new Tokio task.  The sub-agent is given a `MessageTool` so it can push
/// results or progress updates directly to the user without going through the
/// main dispatch loop.  A 5-minute timeout is applied.  Suitable for long-running
/// or IO-heavy tasks (web search, email scanning, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// Fast task — route through the normal `MessageBus` inbound pipeline.
    Quick,
    /// Long task — spawn a dedicated sub-agent with `MessageTool` injected.
    Spawn,
}

/// A parsed heartbeat task with its execution kind.
#[derive(Debug, Clone)]
pub struct HeartbeatTask {
    /// The task description extracted from the `- [ ] …` line.
    pub description: String,
    /// How to execute this task.
    pub kind: TaskKind,
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

pub struct Scheduler;

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        Self
    }

    /// 基础 cron 定时器（每分钟 tick，占位实现）
    pub async fn start(&self) -> Result<()> {
        let mut tick_interval = interval(Duration::from_secs(60));

        info!("Starting cron scheduler...");

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    debug!("Cron tick");
                }
            }
        }
    }
}

// ── Heartbeat 调度器 ──────────────────────────────────────────────────────────

/// Heartbeat 调度器
///
/// 按配置间隔读取 HEARTBEAT.md 并将任务注入到 Agent 调度循环。
///
/// ## 渠道解析优先级
///
/// 1. `config.target_channel` 静态指定 → 直接使用（`session_id = "{channel}:heartbeat"`）
/// 2. `StateManager.get_last_session_id()` → 动态读取最近活跃会话（同 picoclaw 行为）
/// 3. 回退 → `"system:heartbeat"`（dispatch loop 会静默丢弃，不回复用户）
pub struct HeartbeatScheduler {
    config: HeartbeatConfig,
    workspace: PathBuf,
    /// 可选的状态管理器，用于在 `target_channel` 未配置时动态查找最近活跃渠道。
    state: Option<Arc<StateManager>>,
    /// Agent registry — used to spawn sub-agents for Long tasks.
    registry: Option<Arc<AgentRegistry>>,
    /// Concrete bus reference — used to inject `MessageTool` into Long-task sub-agents
    /// so they can push results directly to the user.
    spawn_bus: Option<Arc<AppMessageBus>>,
}

impl HeartbeatScheduler {
    /// 创建 Heartbeat 调度器（不附带状态管理器）。
    pub fn new(config: HeartbeatConfig, workspace: impl Into<PathBuf>) -> Self {
        Self {
            config,
            workspace: workspace.into(),
            state: None,
            registry: None,
            spawn_bus: None,
        }
    }

    /// 附加状态管理器（builder 风格）。
    ///
    /// 设置后，当 `target_channel` 未配置时，调度器会从 `StateManager`
    /// 读取 `last_session_id` 动态确定心跳回传目标。
    pub fn with_state(mut self, state: Arc<StateManager>) -> Self {
        self.state = Some(state);
        self
    }

    /// Attach an `AgentRegistry` so Long tasks can spawn a dedicated sub-agent.
    ///
    /// Without a registry, Long tasks fall back to Quick behaviour (injected
    /// into the normal `MessageBus` pipeline).
    pub fn with_registry(mut self, registry: Arc<AgentRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Attach the concrete `AppMessageBus` for Long-task `MessageTool` injection.
    ///
    /// Without this, Long tasks fall back to Quick behaviour.
    pub fn with_spawn_bus(mut self, bus: Arc<AppMessageBus>) -> Self {
        self.spawn_bus = Some(bus);
        self
    }

    /// 启动 Heartbeat 调度（在独立 tokio task 中运行）
    pub async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        if !self.config.enabled {
            info!("Heartbeat is disabled");
            return Ok(());
        }

        // 最小间隔 5 分钟
        let interval_mins = self.config.interval_minutes.max(5);
        let interval_secs = interval_mins * 60;

        info!(
            interval_mins = interval_mins,
            has_spawn_support = self.registry.is_some() && self.spawn_bus.is_some(),
            "Heartbeat scheduler started"
        );

        let heartbeat_path = self.resolve_heartbeat_path();
        let target_channel = self.config.target_channel.clone();
        let state = self.state.clone();
        let registry = self.registry.clone();
        let spawn_bus = self.spawn_bus.clone();

        let mut tick = interval(Duration::from_secs(interval_secs));
        tick.tick().await; // 跳过第一个立即触发的 tick

        loop {
            tick.tick().await;

            info!("Heartbeat tick — loading tasks from {}", heartbeat_path.display());

            let content = match std::fs::read_to_string(&heartbeat_path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!("HEARTBEAT.md not found, skipping tick");
                    continue;
                }
                Err(e) => {
                    warn!(path = %heartbeat_path.display(), error = %e, "Failed to read HEARTBEAT.md");
                    continue;
                }
            };

            let typed_tasks = parse_heartbeat_tasks_typed(&content);
            if typed_tasks.is_empty() {
                debug!("No pending heartbeat tasks");
                continue;
            }

            // Resolve target channel once per tick (dynamic tracking)
            let (channel, session_id) = resolve_target(&target_channel, state.as_deref());

            for task in &typed_tasks {
                match task.kind {
                    TaskKind::Quick => {
                        // Quick path: inject into the normal MessageBus pipeline
                        info!(
                            task = %task.description,
                            channel = %channel,
                            kind = "quick",
                            "Dispatching heartbeat task"
                        );
                        if let Err(e) = Self::dispatch_task(
                            &bus,
                            &task.description,
                            &channel,
                            &session_id,
                        ).await {
                            warn!(
                                task = %task.description,
                                error = %e,
                                "Failed to dispatch quick heartbeat task"
                            );
                        }
                    }
                    TaskKind::Spawn => {
                        // Long path: spawn a dedicated sub-agent
                        match (&registry, &spawn_bus) {
                            (Some(reg), Some(sbus)) => {
                                info!(
                                    task = %task.description,
                                    channel = %channel,
                                    kind = "spawn",
                                    "Spawning sub-agent for long heartbeat task"
                                );
                                spawn_long_task(
                                    task.description.clone(),
                                    Arc::clone(reg),
                                    Arc::clone(sbus),
                                    channel.clone(),
                                    session_id.clone(),
                                );
                            }
                            _ => {
                                // Graceful fallback: no registry/bus → treat as Quick
                                debug!(
                                    task = %task.description,
                                    "No spawn support configured, falling back to quick dispatch"
                                );
                                if let Err(e) = Self::dispatch_task(
                                    &bus,
                                    &task.description,
                                    &channel,
                                    &session_id,
                                ).await {
                                    warn!(
                                        task = %task.description,
                                        error = %e,
                                        "Failed to dispatch fallback long task as quick"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// 解析 HEARTBEAT.md 文件路径
    fn resolve_heartbeat_path(&self) -> PathBuf {
        if let Some(custom_path) = &self.config.heartbeat_file {
            PathBuf::from(custom_path)
        } else {
            self.workspace.join("HEARTBEAT.md")
        }
    }

    /// 将单个 Heartbeat 任务作为 InboundMessage 注入总线。
    ///
    /// - `channel`    — 渠道名（如 `"telegram"`），决定 Agent 回复发往哪里
    /// - `session_id` — 会话 ID（如 `"telegram:123456789"`），决定回复给哪个用户
    async fn dispatch_task(
        bus: &Arc<dyn MessageBus>,
        task: &str,
        channel: &str,
        session_id: &str,
    ) -> Result<()> {
        let msg = InboundMessage {
            id: Uuid::new_v4(),
            channel: channel.to_string(),
            session_id: session_id.to_string(),
            sender_id: "heartbeat".to_string(),
            sender_name: "Heartbeat Scheduler".to_string(),
            content: MessageContent::Text(task.to_string()),
            reply_to: None,
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "source".to_string(),
                    serde_json::Value::String("heartbeat".to_string()),
                );
                m
            },
        };

        bus.send_inbound(msg).await?;
        Ok(())
    }
}

// ── 渠道解析辅助函数 ───────────────────────────────────────────────────────────

/// 解析 Heartbeat 的目标渠道和会话 ID。
///
/// ## 优先级（对标 picoclaw HeartbeatService）
///
/// 1. `target_channel` 静态配置 → `(channel, "{channel}:heartbeat")`
/// 2. `state.get_last_session_id()` 动态读取 → 解析 `"platform:user_id"` 格式
/// 3. 回退 → `("system:heartbeat", "heartbeat:fallback")`（消息注入总线但不回复）
///
/// ## 内部渠道过滤
///
/// 以 `"system"` 开头或等于 `"cli"` / `"heartbeat"` 的 session_id 会被跳过，
/// 防止心跳循环触发（即 Heartbeat 结果不能作为新的 Heartbeat 目标）。
fn resolve_target(
    target_channel: &Option<String>,
    state: Option<&StateManager>,
) -> (String, String) {
    // 优先级 1：静态配置
    if let Some(ch) = target_channel {
        let session_id = format!("{}:heartbeat", ch);
        return (ch.clone(), session_id);
    }

    // 优先级 2：动态读取 StateManager
    if let Some(state) = state {
        if let Some(last_session) = state.get_last_session_id() {
            // 解析 "platform:user_or_chat_id" 格式
            let channel_part = last_session
                .splitn(2, ':')
                .next()
                .unwrap_or("")
                .to_string();

            // 过滤内部渠道（防止心跳自循环）
            if !is_internal_channel(&channel_part) && !channel_part.is_empty() {
                debug!(
                    last_session = %last_session,
                    channel = %channel_part,
                    "Heartbeat resolved target from StateManager"
                );
                return (channel_part, last_session);
            }
        }
    }

    // 优先级 3：回退（总线会接收但 system:heartbeat 渠道无法回复用户）
    debug!("Heartbeat: no active channel found, using system fallback");
    ("system:heartbeat".to_string(), "heartbeat:fallback".to_string())
}

/// 判断渠道名是否为内部渠道（不应作为 Heartbeat 回传目标）。
///
/// 内部渠道：`"system"`、`"cli"`、`"heartbeat"` 及以 `"system:"` 为前缀的渠道。
fn is_internal_channel(channel: &str) -> bool {
    matches!(channel, "system" | "cli" | "heartbeat")
        || channel.starts_with("system:")
}

// ── HEARTBEAT.md 解析 ─────────────────────────────────────────────────────────

/// 从 HEARTBEAT.md 中提取未完成的任务（`- [ ] 任务描述`）
///
/// - 跳过已完成的任务（`- [x]` 或 `- [X]`）
/// - 跳过空行和注释行
/// - 返回任务描述字符串列表
pub fn load_heartbeat_tasks(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    // 拒绝符号链接
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        warn!(path = %path.display(), "Refusing to load HEARTBEAT.md from symlink");
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)?;
    let tasks = parse_heartbeat_tasks(&content);
    Ok(tasks)
}

/// 从 Markdown 文本中提取未完成任务
pub fn parse_heartbeat_tasks(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // 匹配 `- [ ] 任务描述` 格式（未完成任务）
            if let Some(rest) = trimmed
                .strip_prefix("- [ ] ")
                .or_else(|| trimmed.strip_prefix("* [ ] "))
            {
                let task = rest.trim().to_string();
                if task.is_empty() {
                    None
                } else {
                    Some(task)
                }
            } else {
                None // 跳过已完成 [x] 和其他行
            }
        })
        .collect()
}

// ── Typed task parser ─────────────────────────────────────────────────────────

/// Parse HEARTBEAT.md content and return typed tasks with their execution kind.
///
/// ## Section rules
///
/// | Section header (case-insensitive prefix) | Assigned kind   |
/// |------------------------------------------|-----------------|
/// | `## Long Tasks` / `## Long`               | `TaskKind::Spawn` |
/// | `## Quick Tasks` / `## Quick` / anything else | `TaskKind::Quick` |
/// | No section header (top-level tasks)       | `TaskKind::Quick` |
///
/// Tasks within a section keep that section's kind.  When no header has been
/// seen yet, tasks default to `Quick`.
///
/// ## Example HEARTBEAT.md
///
/// ```markdown
/// # Daily Tasks
///
/// ## Quick Tasks
/// - [ ] Report current time and uptime
/// - [ ] Send daily greeting
///
/// ## Long Tasks
/// - [ ] Search the web for AI news and summarize
/// - [ ] Scan unread emails and highlight urgent ones
///
/// ## Completed
/// - [x] Setup complete
/// ```
pub fn parse_heartbeat_tasks_typed(content: &str) -> Vec<HeartbeatTask> {
    let mut tasks = Vec::new();
    let mut current_kind = TaskKind::Quick; // default for tasks before any section header

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect section headers (## level-2 headers)
        if trimmed.starts_with("## ") {
            let header = trimmed[3..].trim().to_lowercase();
            if header.starts_with("long") {
                current_kind = TaskKind::Spawn;
            } else {
                current_kind = TaskKind::Quick;
            }
            continue;
        }

        // Skip # level-1 headers
        if trimmed.starts_with("# ") {
            continue;
        }

        // Match `- [ ] task` or `* [ ] task` (pending tasks only)
        let description = trimmed
            .strip_prefix("- [ ] ")
            .or_else(|| trimmed.strip_prefix("* [ ] "))
            .map(str::trim)
            .filter(|t| !t.is_empty());

        if let Some(desc) = description {
            tasks.push(HeartbeatTask {
                description: desc.to_string(),
                kind: current_kind.clone(),
            });
        }
    }

    tasks
}

// ── Long task spawning ────────────────────────────────────────────────────────

/// Spawn a dedicated sub-agent for a Long heartbeat task.
///
/// ## Execution model
///
/// 1. Look up the default agent from `registry`.
/// 2. Build that agent's tool list and prepend a `MessageTool` wired to
///    `spawn_bus`/`channel`/`session_id` so the sub-agent can push results
///    directly to the user.
/// 3. Run `AgentEngine::run_tool_loop_with_options()` inside a new Tokio task
///    with a **5-minute timeout**.
/// 4. If the agent calls `message(…)` during execution, the user receives
///    progress updates in real-time.
/// 5. When the loop exits (success or error), the final response is sent to
///    the user as a follow-up message.
///
/// ## No registry fallback
///
/// If no default agent is available, a warning is logged and the task is
/// silently dropped.  The caller (`start()`) already validated that registry
/// and spawn_bus are `Some` before calling this function.
fn spawn_long_task(
    task: String,
    registry: Arc<AgentRegistry>,
    spawn_bus: Arc<AppMessageBus>,
    channel: String,
    session_id: String,
) {
    tokio::spawn(async move {
        // ── 1. Get the default agent ──────────────────────────────────────────
        let agent = match registry.get_default() {
            Some(a) => a,
            None => {
                warn!("Long task: no default agent available, dropping task");
                return;
            }
        };

        // ── 2. Build tool list + inject MessageTool ───────────────────────────
        let mut tools = agent.build_tools();
        tools.insert(0, Box::new(MessageTool::new(
            Arc::clone(&spawn_bus),
            channel.clone(),
            session_id.clone(),
        )));

        let provider  = Arc::clone(&agent.provider);
        let model     = agent.model.clone();
        let temp      = agent.temperature;
        let max_iter  = agent.max_iterations;
        // Append a hint to the system prompt so the agent uses MessageTool
        let system_extra = {
            let base = agent.system_extra.clone().unwrap_or_default();
            let hint = "\n\nIMPORTANT: You are running as a background task. \
                        Use the `message` tool to deliver your final result and any \
                        intermediate progress updates to the user.";
            Some(if base.is_empty() {
                hint.trim_start_matches('\n').to_string()
            } else {
                format!("{}{}", base, hint)
            })
        };

        debug!(
            task = %task.chars().take(80).collect::<String>(),
            model = %model,
            "Long task sub-agent starting"
        );

        // ── 3. Run with 5-minute timeout ──────────────────────────────────────
        let engine = AgentEngine::new();
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            engine.run_tool_loop_with_options(
                provider.as_ref(),
                &tools,
                &task,
                &model,
                temp,
                system_extra.as_deref(),
                Some(max_iter),
            ),
        ).await;

        // ── 4. Send final status to user ─────────────────────────────────────
        // The agent may have already called `message` for intermediate updates.
        // We send the final engine return value as a summary/completion notice.
        let final_text = match result {
            Ok(Ok(resp)) => {
                // Agent completed successfully.  If the response is just a
                // brief "done" (agent used MessageTool for the real output),
                // we suppress a redundant message; otherwise surface it.
                if resp.trim().len() > 20 {
                    Some(resp)
                } else {
                    None // MessageTool already delivered the result
                }
            }
            Ok(Err(e)) => Some(format!("❌ Background task failed: {}", e)),
            Err(_)     => Some("⏰ Background task timed out after 5 minutes.".to_string()),
        };

        if let Some(text) = final_text {
            let out = OutboundMessage {
                id: Uuid::new_v4(),
                target_channel: channel,
                target_session_id: session_id,
                content: MessageContent::Text(
                    adaclaw_security::scrub::scrub_credentials(&text)
                ),
                reply_to: None,
            };
            if let Err(e) = spawn_bus.send_outbound(out) {
                warn!(error = %e, "Long task: failed to send final result to user");
            }
        }
    });
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pending_tasks() {
        let md = r#"
# Heartbeat Tasks

- [ ] 检查今日天气并发送早报
- [x] 已完成的任务
- [X] 另一个已完成任务
- [ ] 扫描未读邮件摘要
* [ ] 星号格式也支持

## Completed

- [x] 安装完成
"#;
        let tasks = parse_heartbeat_tasks(md);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], "检查今日天气并发送早报");
        assert_eq!(tasks[1], "扫描未读邮件摘要");
        assert_eq!(tasks[2], "星号格式也支持");
    }

    #[test]
    fn test_parse_empty_content() {
        let tasks = parse_heartbeat_tasks("");
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_parse_no_pending_tasks() {
        let md = "- [x] All done\n- [X] Also done\n";
        let tasks = parse_heartbeat_tasks(md);
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_load_nonexistent_file() {
        let path = Path::new("/nonexistent/HEARTBEAT.md");
        let tasks = load_heartbeat_tasks(path).unwrap();
        assert!(tasks.is_empty());
    }

    // ── parse_heartbeat_tasks_typed tests ─────────────────────────────────────

    #[test]
    fn test_typed_parser_no_header_defaults_to_quick() {
        let md = "- [ ] check status\n- [ ] send greeting\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.kind == TaskKind::Quick));
    }

    #[test]
    fn test_typed_parser_quick_section() {
        let md = "## Quick Tasks\n- [ ] check weather\n- [ ] send hello\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.kind == TaskKind::Quick));
    }

    #[test]
    fn test_typed_parser_long_section() {
        let md = "## Long Tasks\n- [ ] search web for news\n- [ ] scan emails\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.kind == TaskKind::Spawn));
    }

    #[test]
    fn test_typed_parser_mixed_sections() {
        let md = r#"
# Daily Tasks

## Quick Tasks
- [ ] Report current time
- [ ] Send greeting

## Long Tasks
- [ ] Search AI news
- [ ] Scan unread emails

## Completed
- [x] Setup done
"#;
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].kind, TaskKind::Quick);
        assert_eq!(tasks[0].description, "Report current time");
        assert_eq!(tasks[1].kind, TaskKind::Quick);
        assert_eq!(tasks[1].description, "Send greeting");
        assert_eq!(tasks[2].kind, TaskKind::Spawn);
        assert_eq!(tasks[2].description, "Search AI news");
        assert_eq!(tasks[3].kind, TaskKind::Spawn);
        assert_eq!(tasks[3].description, "Scan unread emails");
    }

    #[test]
    fn test_typed_parser_long_section_chinese() {
        let md = "## Long 任务\n- [ ] 扫描未读邮件\n- [ ] 搜索AI新闻\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 2);
        // Header starts with "long" (case-insensitive) → Spawn
        assert!(tasks.iter().all(|t| t.kind == TaskKind::Spawn));
    }

    #[test]
    fn test_typed_parser_skips_completed() {
        let md = "## Long Tasks\n- [ ] pending task\n- [x] done task\n- [X] also done\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].description, "pending task");
        assert_eq!(tasks[0].kind, TaskKind::Spawn);
    }

    #[test]
    fn test_typed_parser_section_resets_kind() {
        let md = "## Long Tasks\n- [ ] long job\n## Quick Tasks\n- [ ] quick job\n";
        let tasks = parse_heartbeat_tasks_typed(md);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].kind, TaskKind::Spawn);
        assert_eq!(tasks[1].kind, TaskKind::Quick);
    }
}
