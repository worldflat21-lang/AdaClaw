//! Cron 调度器 + Heartbeat 系统（Phase 10）
//!
//! ## Heartbeat
//!
//! 读取工作区根目录的 `HEARTBEAT.md`（或配置指定路径），
//! 提取 `- [ ] 任务描述` 行，按配置间隔触发一次 Agent 执行。
//!
//! 结果通过 MessageBus 回传到指定渠道（`target_channel`）。
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

use crate::config::schema::HeartbeatConfig;
use adaclaw_core::channel::{InboundMessage, MessageContent, MessageBus};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, info, warn};
use uuid::Uuid;

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
pub struct HeartbeatScheduler {
    config: HeartbeatConfig,
    workspace: PathBuf,
}

impl HeartbeatScheduler {
    pub fn new(config: HeartbeatConfig, workspace: impl Into<PathBuf>) -> Self {
        Self {
            config,
            workspace: workspace.into(),
        }
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
            "Heartbeat scheduler started"
        );

        let heartbeat_path = self.resolve_heartbeat_path();
        let target_channel = self.config.target_channel.clone();

        let mut tick = interval(Duration::from_secs(interval_secs));
        tick.tick().await; // 跳过第一个立即触发的 tick

        loop {
            tick.tick().await;

            info!("Heartbeat tick — loading tasks from {}", heartbeat_path.display());

            match load_heartbeat_tasks(&heartbeat_path) {
                Ok(tasks) if tasks.is_empty() => {
                    debug!("No pending heartbeat tasks");
                }
                Ok(tasks) => {
                    for task in &tasks {
                        info!(task = %task, "Dispatching heartbeat task");
                        if let Err(e) = Self::dispatch_task(&bus, task, &target_channel).await {
                            warn!(task = %task, error = %e, "Failed to dispatch heartbeat task");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        path = %heartbeat_path.display(),
                        error = %e,
                        "Failed to load HEARTBEAT.md"
                    );
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

    /// 将单个 Heartbeat 任务作为 InboundMessage 注入总线
    async fn dispatch_task(
        bus: &Arc<dyn MessageBus>,
        task: &str,
        target_channel: &Option<String>,
    ) -> Result<()> {
        let channel = target_channel
            .as_deref()
            .unwrap_or("system:heartbeat")
            .to_string();

        let msg = InboundMessage {
            id: Uuid::new_v4(),
            channel: channel.clone(),
            session_id: format!("heartbeat:{}", Uuid::new_v4()),
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
}
