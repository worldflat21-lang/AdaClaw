//! `AgentRouter` — 基于配置规则的消息路由
//!
//! # 三级路由优先级
//!
//! 规则按优先级从高到低匹配，**第一条匹配的规则生效**：
//!
//! | 优先级 | 规则字段 | 说明 |
//! |--------|----------|------|
//! | **1** | `channel_pattern` | Glob 模式匹配消息来源渠道（如 `telegram:*`、`cli`） |
//! | **2** | `sender_id` / `sender_name` | 精确匹配发送者 ID，或 Glob 匹配显示名称 |
//! | **3** | `default = true` | 兜底规则（必须放在最后） |
//!
//! # 特殊通道
//!
//! `channel = "system"` 的消息是 sub-agent 结果回传通道，**不经过路由规则**，
//! 由 daemon 处理循环在调用 `route()` 之前直接拦截。

use crate::config::schema::RoutingRule;
use adaclaw_core::channel::InboundMessage;
use regex::Regex;

pub struct AgentRouter {
    rules: Vec<RoutingRule>,
}

impl AgentRouter {
    pub fn new(rules: Vec<RoutingRule>) -> Self {
        Self { rules }
    }

    /// 按三级优先级路由消息，返回目标 Agent ID。
    ///
    /// - 若没有任何规则匹配，返回 `None`（调用方应 fallback 到默认 Agent）。
    /// - `channel = "system"` 的消息由上层拦截，不应传入此方法。
    pub fn route(&self, msg: &InboundMessage) -> Option<String> {
        for rule in &self.rules {
            // ── Priority 1: channel_pattern（Glob → Regex 转换）──────────────
            if let Some(pattern) = &rule.channel_pattern {
                let regex_pattern = glob_to_regex(pattern);
                if let Ok(re) = Regex::new(&format!("^{}$", regex_pattern)) {
                    if re.is_match(&msg.channel) {
                        return Some(rule.agent.clone());
                    }
                }
                // channel_pattern 不匹配时继续检查下一规则（不检查同规则的其他字段）
                continue;
            }

            // ── Priority 2: sender_id（精确匹配）或 sender_name（Glob）────────
            if let Some(sender) = &rule.sender_id {
                if sender == &msg.sender_id {
                    return Some(rule.agent.clone());
                }
                continue;
            }

            if let Some(name_pattern) = &rule.sender_name {
                let regex_pattern = glob_to_regex(name_pattern);
                if let Ok(re) = Regex::new(&format!("^{}$", regex_pattern)) {
                    if re.is_match(&msg.sender_name) {
                        return Some(rule.agent.clone());
                    }
                }
                continue;
            }

            // ── Priority 3: default（兜底）────────────────────────────────────
            if rule.default {
                return Some(rule.agent.clone());
            }
        }
        None
    }

    /// 路由消息，若无匹配规则则返回 `fallback_agent`。
    pub fn route_or(&self, msg: &InboundMessage, fallback_agent: &str) -> String {
        self.route(msg)
            .unwrap_or_else(|| fallback_agent.to_string())
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 将 Glob 模式转换为等效的正则表达式片段。
///
/// 仅处理 `*`（任意字符序列）和 `?`（单个字符），其余字符进行正则转义。
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::with_capacity(glob.len() * 2);
    for ch in glob.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            // 转义正则特殊字符
            '.' | '+' | '^' | '$' | '{' | '}' | '[' | ']' | '(' | ')' | '|' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            _ => regex.push(ch),
        }
    }
    regex
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use adaclaw_core::channel::MessageContent;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn make_msg(channel: &str, sender_id: &str, sender_name: &str) -> InboundMessage {
        InboundMessage {
            id: Uuid::new_v4(),
            channel: channel.to_string(),
            session_id: "test-session".to_string(),
            sender_id: sender_id.to_string(),
            sender_name: sender_name.to_string(),
            content: MessageContent::Text("hello".to_string()),
            reply_to: None,
            metadata: HashMap::new(),
        }
    }

    fn make_rule(
        channel_pattern: Option<&str>,
        sender_id: Option<&str>,
        sender_name: Option<&str>,
        default: bool,
        agent: &str,
    ) -> RoutingRule {
        RoutingRule {
            channel_pattern: channel_pattern.map(|s| s.to_string()),
            sender_id: sender_id.map(|s| s.to_string()),
            sender_name: sender_name.map(|s| s.to_string()),
            default,
            agent: agent.to_string(),
        }
    }

    #[test]
    fn test_route_by_channel_pattern() {
        let router = AgentRouter::new(vec![
            make_rule(Some("telegram:*"), None, None, false, "coder"),
            make_rule(None, None, None, true, "assistant"),
        ]);
        let msg = make_msg("telegram:@dev_bot", "u1", "Alice");
        assert_eq!(router.route(&msg).as_deref(), Some("coder"));
    }

    #[test]
    fn test_route_by_sender_id() {
        let router = AgentRouter::new(vec![
            make_rule(None, Some("user_123"), None, false, "vip_agent"),
            make_rule(None, None, None, true, "assistant"),
        ]);
        let msg = make_msg("cli", "user_123", "Bob");
        assert_eq!(router.route(&msg).as_deref(), Some("vip_agent"));
    }

    #[test]
    fn test_route_by_sender_name_glob() {
        let router = AgentRouter::new(vec![
            make_rule(None, None, Some("admin_*"), false, "admin_agent"),
            make_rule(None, None, None, true, "assistant"),
        ]);
        let msg = make_msg("cli", "u42", "admin_carol");
        assert_eq!(router.route(&msg).as_deref(), Some("admin_agent"));
    }

    #[test]
    fn test_route_default_fallback() {
        let router = AgentRouter::new(vec![
            make_rule(None, None, None, true, "assistant"),
        ]);
        let msg = make_msg("cli", "u1", "Alice");
        assert_eq!(router.route(&msg).as_deref(), Some("assistant"));
    }

    #[test]
    fn test_route_no_match_returns_none() {
        let router = AgentRouter::new(vec![
            make_rule(Some("telegram:*"), None, None, false, "coder"),
        ]);
        let msg = make_msg("cli", "u1", "Alice");
        assert_eq!(router.route(&msg), None);
    }

    #[test]
    fn test_route_or_uses_fallback() {
        let router = AgentRouter::new(vec![]);
        let msg = make_msg("cli", "u1", "Alice");
        assert_eq!(router.route_or(&msg, "default_agent"), "default_agent");
    }

    #[test]
    fn test_priority_channel_over_sender() {
        // channel_pattern has higher priority than sender_id
        let router = AgentRouter::new(vec![
            make_rule(Some("telegram:*"), None, None, false, "coder"),
            make_rule(None, Some("u1"), None, false, "personal"),
            make_rule(None, None, None, true, "assistant"),
        ]);
        let msg = make_msg("telegram:@dev_bot", "u1", "Alice");
        assert_eq!(router.route(&msg).as_deref(), Some("coder"));
    }

    #[test]
    fn test_glob_to_regex() {
        assert_eq!(glob_to_regex("telegram:*"), "telegram:.*");
        assert_eq!(glob_to_regex("admin_?"), "admin_.");
        assert_eq!(glob_to_regex("foo.bar"), r"foo\.bar");
    }
}
