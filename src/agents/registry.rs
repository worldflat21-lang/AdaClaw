//! `AgentRegistry` — 多 Agent 实例注册表
//!
//! 存储所有已实例化的 `AgentInstance`，提供：
//! - 按 ID 查找 Agent
//! - 委托权限校验（`can_delegate`）
//! - 默认 Agent 获取（`get_default`）
//! - Agent 列表枚举

use crate::agents::instance::AgentInstance;
use std::collections::HashMap;

/// 多 Agent 运行时注册表。
///
/// 在 daemon 启动时一次性构建，之后以 `Arc<AgentRegistry>` 形式在任务间共享（只读）。
pub struct AgentRegistry {
    /// Agent ID → AgentInstance 映射。
    agents: HashMap<String, AgentInstance>,
    /// 默认 Agent 的 ID（当路由规则无匹配时使用）。
    default_agent_id: String,
}

impl AgentRegistry {
    /// 创建空注册表，并指定默认 Agent ID。
    pub fn new(default_agent_id: impl Into<String>) -> Self {
        Self {
            agents: HashMap::new(),
            default_agent_id: default_agent_id.into(),
        }
    }

    /// 插入一个 `AgentInstance`（以 `agent_id` 为 key）。
    pub fn insert(&mut self, instance: AgentInstance) {
        self.agents.insert(instance.agent_id.clone(), instance);
    }

    /// 按 ID 查找 `AgentInstance`。
    pub fn get(&self, id: &str) -> Option<&AgentInstance> {
        self.agents.get(id)
    }

    /// 返回所有已注册的 Agent ID 列表。
    pub fn list_agents(&self) -> Vec<&str> {
        self.agents.keys().map(|k| k.as_str()).collect()
    }

    /// 检查 `parent_id` 是否被允许委托任务给 `target_id`。
    ///
    /// 返回 `true` 当且仅当：
    /// - `parent_id` 在注册表中存在，且
    /// - `parent.allow_delegate` 包含 `target_id`
    pub fn can_delegate(&self, parent_id: &str, target_id: &str) -> bool {
        self.agents
            .get(parent_id)
            .map(|a| a.allow_delegate.iter().any(|id| id == target_id))
            .unwrap_or(false)
    }

    /// 返回默认 Agent（`default_agent_id`），若不存在则 fallback 到第一个注册的 Agent。
    pub fn get_default(&self) -> Option<&AgentInstance> {
        self.agents
            .get(&self.default_agent_id)
            .or_else(|| self.agents.values().next())
    }

    /// 返回配置的默认 Agent ID。
    pub fn default_agent_id(&self) -> &str {
        &self.default_agent_id
    }

    /// 已注册 Agent 的数量。
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// 注册表是否为空。
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_starts_empty() {
        let reg = AgentRegistry::new("assistant");
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.get("assistant").is_none());
    }

    #[test]
    fn test_default_agent_id() {
        let reg = AgentRegistry::new("assistant");
        assert_eq!(reg.default_agent_id(), "assistant");
    }

    #[test]
    fn test_list_agents_empty() {
        let reg = AgentRegistry::new("assistant");
        assert!(reg.list_agents().is_empty());
    }

    #[test]
    fn test_can_delegate_no_agents() {
        let reg = AgentRegistry::new("assistant");
        assert!(!reg.can_delegate("assistant", "coder"));
    }

    #[test]
    fn test_get_default_fallback_to_first() {
        // "assistant" not in registry but "coder" is
        let reg = AgentRegistry::new("assistant");
        // We can't easily create AgentInstance in tests without a real provider,
        // so we just verify the fallback logic path doesn't panic when empty
        assert!(reg.get_default().is_none());
    }
}
