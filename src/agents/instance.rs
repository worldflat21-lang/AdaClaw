//! `AgentInstance` — 单个 Agent 的完整运行时实例
//!
//! 每个 Agent 拥有：
//! - 独立的 `Arc<dyn Provider>`（不与其他 Agent 共享连接状态）
//! - 按白名单过滤的工具集（`build_tools()` 在运行时重建，规避 `Box<dyn Tool>` 无法 Clone 的限制）
//! - 独立的 `SessionManager`（隔离各 session 的对话历史，留作后续会话持久化扩展）
//! - 专属的工作区目录（默认 `~/.adaclaw/workspace-{agent_id}`）
//! - Sub-agent 委托允许名单（`allow_delegate`，空列表 = 禁止委托，防递归）

use adaclaw_core::memory::Memory;
use adaclaw_core::provider::Provider;
use adaclaw_core::tool::Tool;
use adaclaw_memory::session_store::SessionStore;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

use crate::agents::engine::AgentEngine;
use crate::config::schema::AgentConfig;

// ── ToolRegistry ──────────────────────────────────────────────────────────────

/// 工具注册表：持有一组已实例化的工具，用于列举/检查。
///
/// 由于 `Box<dyn Tool>` 不实现 `Clone`，执行时应调用 `AgentInstance::build_tools()`
/// 获取新鲜的 `Vec<Box<dyn Tool>>`，而非直接从此注册表取用。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// 从已有工具列表构造注册表（消耗所有权）。
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// 返回所有工具的引用切片。
    pub fn tools(&self) -> &[Box<dyn Tool>] {
        &self.tools
    }

    /// 返回所有工具名称（用于白名单检查/日志）。
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// 注册表中工具的数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 注册表是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

// ── SessionManager ────────────────────────────────────────────────────────────

/// 单个 Agent 的会话管理器（按 `session_id` 隔离对话历史）。
///
/// 每个 session 拥有一个持久的 `AgentEngine`，对话历史在多轮消息间保留。
/// 使用 `Arc<AsyncMutex<AgentEngine>>` 保证跨 `tokio::spawn` 共享的同时，
/// 同一 session 的消息被串行化处理（第二条消息等待第一条处理完成再执行）。
pub struct SessionManager {
    /// session_id → 持久 engine（含对话历史）
    sessions: Mutex<HashMap<String, Arc<AsyncMutex<AgentEngine>>>>,
}

impl SessionManager {
    /// 创建空的会话管理器。
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 获取或创建指定 `session_id` 的 `AgentEngine`。
    ///
    /// - 首次调用（新 session）：创建新 engine，若提供 `memory` / `session_store` 则附加。
    /// - 后续调用（已有 session）：返回同一 engine 的 `Arc` 引用，历史记录完整保留。
    ///
    /// 当 `session_store` 提供时，engine 会在创建时自动从 SQLite 恢复历史（记忆续传），
    /// 并在后续每次 `push_history()` 时异步写入 SQLite。
    ///
    /// 返回 `Arc<AsyncMutex<AgentEngine>>`：
    /// - `Arc` 允许在 `tokio::spawn` 闭包中跨线程持有
    /// - `AsyncMutex` 保证同一 session 的多条消息串行执行（`.lock().await` 在 LLM 调用期间持有锁）
    pub fn get_or_create(
        &self,
        session_id: &str,
        memory: Option<Arc<dyn Memory>>,
        session_store: Option<Arc<SessionStore>>,
    ) -> Arc<AsyncMutex<AgentEngine>> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                // Build engine: attach memory first (sets session_id), then
                // attach session_store (which reads session_id for SQLite restore).
                let mut engine = AgentEngine::new();
                if let Some(mem) = memory {
                    engine = engine.with_memory(mem, session_id.to_string());
                }
                if let Some(store) = session_store {
                    engine = engine.with_session_store(store);
                }
                Arc::new(AsyncMutex::new(engine))
            })
            .clone()
    }

    /// 当前活跃 session 数量。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── AgentInstance ─────────────────────────────────────────────────────────────

/// 单个 Agent 的完整运行时实例。
///
/// 通过 [`AgentInstance::new`] 构造，所需的 `Arc<dyn Provider>` 由调用方（daemon）提前创建并传入，
/// 以确保 per-provider API Key 正确绑定。
pub struct AgentInstance {
    /// Agent 唯一标识（对应 `config.toml` 中的 key，如 `"assistant"`）。
    pub agent_id: String,

    /// 独立的 Provider 实例（`Arc` 允许在 spawn 的子任务中共享）。
    pub provider: Arc<dyn Provider>,

    /// 工具注册表（预过滤，用于列举/检查）。
    /// 执行时请调用 `build_tools()` 获取新鲜实例。
    pub tool_registry: ToolRegistry,

    /// 注入的记忆后端（用于 memory_store / memory_recall / memory_forget 工具）。
    pub memory: Option<Arc<dyn Memory>>,

    /// 该 Agent 允许使用的工具名称白名单（空 = 允许所有）。
    pub allowed_tools: Vec<String>,

    /// 允许委托的目标 Agent ID 列表（空 = 禁止委托，防递归）。
    /// `DelegateTool` 仅在此列表非空时注入。
    pub allow_delegate: Vec<String>,

    /// Agent 工作区根目录。
    pub workspace: PathBuf,

    /// LLM 模型名称（直接从 `AgentConfig` 缓存，避免重复查找）。
    pub model: String,

    /// 采样温度。
    pub temperature: f64,

    /// 每轮最大工具调用迭代次数。
    pub max_iterations: usize,

    /// 附加系统提示（追加到默认系统提示之后）。
    pub system_extra: Option<String>,

    /// 会话管理器（按 session_id 隔离，留作后续扩展）。
    pub session_manager: SessionManager,

    /// 对话历史持久化存储（可选）。
    ///
    /// 注入后，`get_or_create_engine()` 会在创建新 engine 时自动调用
    /// `AgentEngine::with_session_store()`，从 SQLite 恢复历史并启用写入持久化。
    pub session_store: Option<Arc<SessionStore>>,
}

impl AgentInstance {
    /// 根据 `AgentConfig` 和已创建的 `Provider` 构建 `AgentInstance`。
    ///
    /// # 工具过滤
    ///
    /// - `config.tools` 为空 → 允许所有系统工具
    /// - `config.tools` 非空 → 只保留白名单内的工具
    /// - `DelegateTool` **不**在此处注入，由 daemon dispatch loop 在运行时按需添加
    ///
    /// # Workspace
    ///
    /// 优先使用 `config.workspace`（支持 `~` 展开），否则默认 `~/.adaclaw/workspace-{agent_id}`。
    /// 目录不存在时自动创建（含所有父目录）。
    pub fn new(agent_id: &str, config: &AgentConfig, provider: Arc<dyn Provider>) -> Result<Self> {
        // ── 1. 构建预过滤工具注册表（仅用于检查，执行时重建） ────────────────
        // memory 此时尚未创建，用 None 构建检查用注册表（不涉及实际执行）
        let all = adaclaw_tools::registry::all_tools(None);
        let filtered: Vec<Box<dyn Tool>> = if config.tools.is_empty() {
            all
        } else {
            let whitelist: HashSet<&str> = config.tools.iter().map(|s| s.as_str()).collect();
            all.into_iter()
                .filter(|t| whitelist.contains(t.name()))
                .collect()
        };
        let tool_registry = ToolRegistry::new(filtered);

        // ── 2. 解析 workspace 路径 ─────────────────────────────────────────────
        let workspace = if let Some(ws) = &config.workspace {
            PathBuf::from(expand_tilde(ws))
        } else {
            default_workspace(agent_id)
        };

        if !workspace.exists() {
            std::fs::create_dir_all(&workspace).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to create workspace '{}': {}",
                    workspace.display(),
                    e
                )
            })?;
        }

        Ok(Self {
            agent_id: agent_id.to_string(),
            provider,
            tool_registry,
            memory: None, // 由调用方通过 with_memory() 或 daemon 注入
            allowed_tools: config.tools.clone(),
            allow_delegate: config.subagents.allow.clone(),
            workspace,
            model: config.model.clone(),
            temperature: config.temperature,
            max_iterations: config.max_iterations,
            system_extra: config.system_extra.clone(),
            session_manager: SessionManager::new(),
            session_store: None, // 由调用方通过 with_session_store() 注入
        })
    }

    /// 重建一个新鲜的工具列表，按 `allowed_tools` 白名单过滤。
    ///
    /// 每次消息处理时调用，规避 `Box<dyn Tool>` 无法 Clone 的限制。
    /// 调用方（daemon dispatch loop）在返回的列表末尾注入 `DelegateTool`（如果允许委托）。
    pub fn build_tools(&self) -> Vec<Box<dyn Tool>> {
        let all = adaclaw_tools::registry::all_tools(self.memory.clone());
        if self.allowed_tools.is_empty() {
            return all;
        }
        let allowed: HashSet<&str> = self.allowed_tools.iter().map(|s| s.as_str()).collect();
        all.into_iter()
            .filter(|t| allowed.contains(t.name()))
            .collect()
    }

    /// 附加记忆后端（builder 风格，供 daemon 在构建后注入）。
    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 附加 SessionStore（builder 风格，供 daemon 在构建后注入）。
    ///
    /// 注入后，此 Agent 的所有新 session 在第一次调用 `get_or_create_engine()` 时
    /// 会自动从 SQLite 恢复历史，并在后续每轮对话结束后异步写入 SQLite。
    pub fn with_session_store(mut self, store: Arc<SessionStore>) -> Self {
        self.session_store = Some(store);
        self
    }

    /// 返回此 Agent 是否允许委托任务给其他 Agent。
    pub fn can_delegate(&self) -> bool {
        !self.allow_delegate.is_empty()
    }

    /// 获取或创建指定 `session_id` 的持久 `AgentEngine`。
    ///
    /// - 首次调用会创建新 engine，自动附加 `memory` 和 `session_store`（若已注入）
    /// - `session_store` 触发从 SQLite 恢复历史（记忆续传）
    /// - 后续调用返回同一 engine，保留全部对话历史
    ///
    /// 返回的 `Arc<AsyncMutex<AgentEngine>>` 可安全传入 `tokio::spawn`；
    /// 持有锁时进行 LLM 调用，天然实现同一 session 内消息的串行化。
    pub fn get_or_create_engine(
        &self,
        session_id: &str,
        memory: Option<Arc<dyn Memory>>,
    ) -> Arc<AsyncMutex<AgentEngine>> {
        self.session_manager
            .get_or_create(session_id, memory, self.session_store.clone())
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 展开 `~` 前缀为用户主目录。
fn expand_tilde(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix('~') {
        let home = home_dir();
        format!("{}{}", home, stripped)
    } else {
        path.to_string()
    }
}

/// 返回默认 workspace 路径：`~/.adaclaw/workspace-{agent_id}`。
fn default_workspace(agent_id: &str) -> PathBuf {
    PathBuf::from(home_dir())
        .join(".adaclaw")
        .join(format!("workspace-{}", agent_id))
}

/// 跨平台获取用户主目录路径字符串。
fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| {
            let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
            let path = std::env::var("HOMEPATH").unwrap_or_default();
            if drive.is_empty() && path.is_empty() {
                Err(std::env::VarError::NotPresent)
            } else {
                Ok(format!("{}{}", drive, path))
            }
        })
        .unwrap_or_else(|_| ".".to_string())
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry_new_and_len() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(adaclaw_tools::shell::ShellTool::new()),
            Box::new(adaclaw_tools::file::FileReadTool::new()),
        ];
        let registry = ToolRegistry::new(tools);
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
    }

    #[test]
    fn test_tool_registry_tool_names() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(adaclaw_tools::shell::ShellTool::new()),
            Box::new(adaclaw_tools::file::FileReadTool::new()),
        ];
        let registry = ToolRegistry::new(tools);
        let names = registry.tool_names();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
    }

    #[test]
    fn test_session_manager_starts_empty() {
        let sm = SessionManager::new();
        assert_eq!(sm.session_count(), 0);
    }

    #[test]
    fn test_default_workspace_contains_agent_id() {
        let ws = default_workspace("coder");
        let ws_str = ws.to_string_lossy();
        assert!(
            ws_str.contains("workspace-coder"),
            "Expected 'workspace-coder' in '{}'",
            ws_str
        );
        assert!(
            ws_str.contains(".adaclaw"),
            "Expected '.adaclaw' in '{}'",
            ws_str
        );
    }

    #[test]
    fn test_default_workspace_different_agents() {
        let ws1 = default_workspace("assistant");
        let ws2 = default_workspace("coder");
        assert_ne!(
            ws1, ws2,
            "Different agents should have different workspaces"
        );
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/projects");
        assert!(!expanded.starts_with('~'), "Tilde should be expanded");
        assert!(
            expanded.ends_with("/projects"),
            "Path suffix should be preserved"
        );
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let path = "/absolute/path";
        assert_eq!(expand_tilde(path), path);
    }
}
