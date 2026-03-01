//! `StateManager` — 守护进程持久化运行状态
//!
//! 对标 picoclaw `pkg/state/state.go`。
//!
//! ## 存储结构
//!
//! ```
//! workspace/
//! └── state/
//!     └── state.json   ← DaemonState（原子写入）
//! ```
//!
//! ## 追踪内容
//!
//! - `last_channel`   — 最近活跃渠道名，如 `"telegram"`
//! - `last_session_id` — 最近活跃会话 ID，如 `"telegram:123456789"`
//!
//! Heartbeat 调度器在 `target_channel` 未配置时读取此状态，
//! 将心跳结果发回给最近与 Agent 交互的用户。
//!
//! ## 线程安全
//!
//! 内部用 `Arc<RwLock<DaemonState>>` 保护，读多写少（每条入站消息写一次）。
//! 文件 I/O 在锁外执行（先 snapshot，再异步写盘）。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tracing::{debug, warn};

// ── DaemonState（可序列化） ────────────────────────────────────────────────────

/// 持久化运行时状态快照。
///
/// 对应 picoclaw 的 `State` 结构体。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DaemonState {
    /// 最近活跃渠道名（如 `"telegram"`、`"discord"`）。
    last_channel: Option<String>,

    /// 最近活跃会话 ID（如 `"telegram:123456789"`）。
    /// 格式：`"{channel}:{user_or_chat_id}"`，与 `InboundMessage.session_id` 一致。
    last_session_id: Option<String>,

    /// 最后一次更新的 Unix 时间戳（秒）。
    updated_at: i64,
}

// ── StateManager ──────────────────────────────────────────────────────────────

/// 守护进程持久化状态管理器。
///
/// # 典型用法
///
/// ```ignore
/// // 初始化（从磁盘加载已有状态）
/// let state = Arc::new(StateManager::new(&workspace_path));
///
/// // 每条入站消息（非内部渠道）更新状态
/// state.update_last_active("telegram", "telegram:123456789");
///
/// // Heartbeat 读取最近活跃会话
/// let session = state.get_last_session_id(); // Some("telegram:123456789")
/// ```
pub struct StateManager {
    state: Arc<RwLock<DaemonState>>,
    state_file: PathBuf,
}

impl StateManager {
    /// 从 `workspace/state/` 目录初始化状态管理器。
    ///
    /// - 自动创建 `state/` 子目录（若不存在）。
    /// - 尝试加载已有的 `state.json`（失败时静默使用空状态）。
    pub fn new(workspace: &Path) -> Self {
        let state_dir = workspace.join("state");
        if let Err(e) = std::fs::create_dir_all(&state_dir) {
            warn!(
                path = %state_dir.display(),
                error = %e,
                "Failed to create state directory"
            );
        }

        let state_file = state_dir.join("state.json");
        let state = load_from_disk(&state_file);

        debug!(
            path = %state_file.display(),
            last_channel = ?state.last_channel,
            last_session_id = ?state.last_session_id,
            "StateManager initialized"
        );

        Self {
            state: Arc::new(RwLock::new(state)),
            state_file,
        }
    }

    // ── 读操作 ────────────────────────────────────────────────────────────────

    /// 获取最近活跃渠道名（如 `"telegram"`）。
    pub fn get_last_channel(&self) -> Option<String> {
        self.state
            .read()
            .ok()
            .and_then(|s| s.last_channel.clone())
    }

    /// 获取最近活跃会话 ID（如 `"telegram:123456789"`）。
    pub fn get_last_session_id(&self) -> Option<String> {
        self.state
            .read()
            .ok()
            .and_then(|s| s.last_session_id.clone())
    }

    // ── 写操作 ────────────────────────────────────────────────────────────────

    /// 更新最近活跃渠道和会话（每条真实入站消息调用一次）。
    ///
    /// - `channel`    — 渠道名，如 `"telegram"`
    /// - `session_id` — 会话 ID，如 `"telegram:123456789"`
    ///
    /// 此方法会**异步写入磁盘**（spawn_blocking），不阻塞消息处理循环。
    /// 若写入失败，只打印警告，不影响运行时行为。
    pub fn update_last_active(&self, channel: &str, session_id: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 先读取，避免在没有变化时做无意义的写入
        {
            let current = self.state.read().ok();
            if let Some(s) = current {
                if s.last_channel.as_deref() == Some(channel)
                    && s.last_session_id.as_deref() == Some(session_id)
                {
                    return; // 无变化，跳过
                }
            }
        }

        // 更新内存状态
        let snapshot = {
            let mut state = match self.state.write() {
                Ok(s) => s,
                Err(e) => {
                    warn!("StateManager write lock poisoned: {}", e);
                    return;
                }
            };
            state.last_channel = Some(channel.to_string());
            state.last_session_id = Some(session_id.to_string());
            state.updated_at = now;
            state.clone()
        };

        // 异步写入磁盘，不阻塞调用方
        let state_file = self.state_file.clone();
        tokio::spawn(async move {
            if let Err(e) = save_to_disk(&state_file, &snapshot) {
                warn!(
                    path = %state_file.display(),
                    error = %e,
                    "Failed to persist daemon state"
                );
            } else {
                debug!(
                    channel = %snapshot.last_channel.as_deref().unwrap_or(""),
                    session_id = %snapshot.last_session_id.as_deref().unwrap_or(""),
                    "Daemon state persisted"
                );
            }
        });
    }
}

// ── 磁盘 I/O 辅助函数 ─────────────────────────────────────────────────────────

/// 从磁盘加载状态（失败时返回空状态）。
fn load_from_disk(path: &Path) -> DaemonState {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<DaemonState>(&content) {
            Ok(state) => state,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to parse state.json, using empty state");
                DaemonState::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 第一次运行，正常情况
            DaemonState::default()
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read state.json, using empty state");
            DaemonState::default()
        }
    }
}

/// 原子写入状态到磁盘（tmp → rename）。
///
/// 使用 temp 文件 + rename 模式，确保写入过程中进程崩溃不会产生半损坏文件。
fn save_to_disk(path: &Path, state: &DaemonState) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;

    // 创建同目录下的临时文件
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp_path = dir.join(format!("state-{}.tmp", std::process::id()));

    std::fs::write(&tmp_path, json.as_bytes())?;

    // 原子替换：rename 在 POSIX 上是原子的；在 Windows 上会覆盖目标
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        // rename 失败时清理 tmp
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    Ok(())
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_new_empty_state() {
        let dir = tempdir().unwrap();
        let sm = StateManager::new(dir.path());
        assert!(sm.get_last_channel().is_none());
        assert!(sm.get_last_session_id().is_none());
    }

    #[tokio::test]
    async fn test_update_and_read() {
        let dir = tempdir().unwrap();
        let sm = StateManager::new(dir.path());

        sm.update_last_active("telegram", "telegram:123456");
        // 给异步写入一点时间
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(sm.get_last_channel(), Some("telegram".to_string()));
        assert_eq!(
            sm.get_last_session_id(),
            Some("telegram:123456".to_string())
        );
    }

    #[tokio::test]
    async fn test_state_persists_across_restart() {
        let dir = tempdir().unwrap();

        {
            let sm = StateManager::new(dir.path());
            sm.update_last_active("discord", "discord:987654");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // 模拟重启：重新创建 StateManager
        let sm2 = StateManager::new(dir.path());
        assert_eq!(sm2.get_last_channel(), Some("discord".to_string()));
        assert_eq!(
            sm2.get_last_session_id(),
            Some("discord:987654".to_string())
        );
    }

    #[tokio::test]
    async fn test_no_write_when_unchanged() {
        let dir = tempdir().unwrap();
        let sm = StateManager::new(dir.path());

        sm.update_last_active("telegram", "telegram:111");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mtime1 = std::fs::metadata(dir.path().join("state").join("state.json"))
            .and_then(|m| m.modified())
            .ok();

        // 更新相同的值，不应触发写入
        sm.update_last_active("telegram", "telegram:111");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mtime2 = std::fs::metadata(dir.path().join("state").join("state.json"))
            .and_then(|m| m.modified())
            .ok();

        // mtime 应该相同（没有重写）
        assert_eq!(mtime1, mtime2, "should not write when state unchanged");
    }

    #[tokio::test]
    async fn test_update_overwrites_previous() {
        let dir = tempdir().unwrap();
        let sm = StateManager::new(dir.path());

        sm.update_last_active("telegram", "telegram:111");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sm.update_last_active("slack", "slack:aaa");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(sm.get_last_channel(), Some("slack".to_string()));
        assert_eq!(sm.get_last_session_id(), Some("slack:aaa".to_string()));
    }
}
