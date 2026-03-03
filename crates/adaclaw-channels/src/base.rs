//! `BaseChannel` — 所有渠道共享的辅助结构体
//!
//! 提供：
//! - `is_allowed()` — 白名单检查（支持 "id|username" 复合格式 + glob `*` 通配符）
//! - `handle_message()` — 统一的消息上报到 Bus
//! - `is_running()` / `set_running()` — AtomicBool 状态跟踪

use adaclaw_core::channel::{InboundMessage, MessageBus, MessageContent};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

// ── Glob 通配符匹配（来自 moltis gating.rs，适配 AdaClaw）──────────────────────

/// 支持 `*` 通配符的简单 glob 匹配。
///
/// `*` 可匹配任意长度的字符序列（包括空串）。
/// `pattern` 和 `text` 均应提前 `.to_lowercase()`（调用方负责大小写折叠）。
///
/// 示例（调用方已转小写）：
/// - `"admin_*"` 匹配 `"admin_alice"`
/// - `"*@example.com"` 匹配 `"user@example.com"`
/// - `"user_*_admin"` 匹配 `"user_123_admin"`
fn glob_match_pattern(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    // 没有 * — 直接精确比较
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0usize; // 当前在 text 中已消费到的字节位置

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(idx) => {
                // 第一段必须从 text 开头匹配（pattern 不以 * 开头）
                if i == 0 && idx != 0 {
                    return false;
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }

    // 最后一段必须恰好到 text 末尾（除非 pattern 以 * 结尾）
    let last = parts.last().copied().unwrap_or("");
    if last.is_empty() {
        true
    } else {
        pos == text.len()
    }
}

// ── BaseChannel ───────────────────────────────────────────────────────────────

pub struct BaseChannel {
    pub name: String,
    /// per-channel 发送者白名单（空 = 放行所有人）
    pub allow_from: Vec<String>,
    /// 群组聊天白名单（空 = 回退到 allow_from）
    pub allow_from_groups: Vec<String>,
    /// 群组中是否要求 @提及才响应
    pub require_mention: bool,
    running: Arc<AtomicBool>,
}

impl BaseChannel {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            allow_from: Vec::new(),
            allow_from_groups: Vec::new(),
            require_mention: false,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_allow_from(mut self, allow_from: Vec<String>) -> Self {
        self.allow_from = allow_from;
        self
    }

    pub fn with_group_config(
        mut self,
        allow_from_groups: Vec<String>,
        require_mention: bool,
    ) -> Self {
        self.allow_from_groups = allow_from_groups;
        self.require_mention = require_mention;
        self
    }

    // ── 运行状态 ──────────────────────────────────────────────────────────────

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn set_running(&self, value: bool) {
        self.running.store(value, Ordering::SeqCst);
    }

    // ── 白名单检查 ────────────────────────────────────────────────────────────

    /// 检查 sender_id 是否在白名单中。
    ///
    /// # 规则
    /// 1. `allow_from` 为空 → 放行所有人
    /// 2. 支持精确匹配（`"123456"`）
    /// 3. 支持 `"id|username"` 复合格式（来自 picoclaw 设计）：
    ///    - `"123456"` 可匹配 `"123456|alice"`
    ///    - `"alice"` 或 `"@alice"` 可匹配 `"123456|alice"`
    /// 4. 支持 glob `*` 通配符（来自 moltis gating.rs）：
    ///    - `"admin_*"` 可匹配 `"admin_alice"`、`"admin_bob"` 等
    ///    - `"*@corp.com"` 可匹配 `"user@corp.com"`
    ///    - 通配符匹配不区分大小写
    pub fn is_allowed(&self, sender_id: &str) -> bool {
        if self.allow_from.is_empty() {
            return true;
        }
        self.matches_allowlist(sender_id, &self.allow_from)
    }

    /// 检查群组发送者是否被允许。
    pub fn is_group_allowed(&self, sender_id: &str) -> bool {
        let list = if self.allow_from_groups.is_empty() {
            &self.allow_from
        } else {
            &self.allow_from_groups
        };
        if list.is_empty() {
            return true;
        }
        self.matches_allowlist(sender_id, list)
    }

    fn matches_allowlist(&self, sender_id: &str, list: &[String]) -> bool {
        // 拆解 "id|username" 格式（来自 picoclaw 设计）
        let (id_part, user_part) = if let Some(idx) = sender_id.find('|') {
            (&sender_id[..idx], &sender_id[idx + 1..])
        } else {
            (sender_id, "")
        };

        for allowed in list {
            // 去掉 "@" 前缀（允许配置 "@alice"）
            let trimmed = allowed.trim_start_matches('@');

            if trimmed.contains('*') {
                // ── Glob 通配符匹配（来自 moltis gating.rs）──────────────────
                // 对 sender_id 的全体、id 部分、username 部分分别做不区分大小写的 glob 匹配
                let pat = trimmed.to_lowercase();
                if glob_match_pattern(&pat, &sender_id.to_lowercase())
                    || glob_match_pattern(&pat, &id_part.to_lowercase())
                    || (!user_part.is_empty()
                        && glob_match_pattern(&pat, &user_part.to_lowercase()))
                {
                    return true;
                }
            } else {
                // ── 精确匹配（原有逻辑，保持大小写敏感的数字 ID 匹配）─────────
                if sender_id == allowed
                    || sender_id == trimmed
                    || id_part == allowed
                    || id_part == trimmed
                    || (!user_part.is_empty()
                        && (user_part == allowed || user_part == trimmed))
                {
                    return true;
                }
            }
        }
        false
    }

    // ── 消息上报 ──────────────────────────────────────────────────────────────

    /// 检查白名单并将消息发布到 MessageBus。
    ///
    /// `session_id` 应包含足够的路由信息，以便出站消息能路由回正确的目标：
    /// - Telegram: `"{chat_id}"`（数字字符串）
    /// - DingTalk: sessionWebhook URL
    /// - Discord: `"{channel_id}"`
    /// - Slack: `"{channel_id}"`
    pub async fn handle_message(
        &self,
        bus: &Arc<dyn MessageBus>,
        sender_id: &str,
        sender_name: &str,
        session_id: &str,
        content: &str,
        metadata: HashMap<String, Value>,
    ) {
        if !self.is_allowed(sender_id) {
            tracing::warn!(
                channel = %self.name,
                sender_id = %sender_id,
                "Access denied: sender not in allowlist. \
                 Add to channels.{}.allow_from to grant access.",
                self.name
            );
            return;
        }

        let msg = InboundMessage {
            id: Uuid::new_v4(),
            channel: self.name.clone(),
            session_id: session_id.to_string(),
            sender_id: sender_id.to_string(),
            sender_name: sender_name.to_string(),
            content: MessageContent::Text(content.to_string()),
            reply_to: None,
            metadata,
        };

        if let Err(e) = bus.send_inbound(msg).await {
            tracing::error!(
                channel = %self.name,
                sender_id = %sender_id,
                error = %e,
                "Failed to publish message to bus"
            );
        }
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── glob_match_pattern ────────────────────────────────────────────────────

    #[test]
    fn glob_no_wildcard_exact_match() {
        assert!(glob_match_pattern("alice", "alice"));
        assert!(!glob_match_pattern("alice", "bob"));
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(glob_match_pattern("admin_*", "admin_alice"));
        assert!(glob_match_pattern("admin_*", "admin_"));
        assert!(!glob_match_pattern("admin_*", "user_bob"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(glob_match_pattern("*@example.com", "user@example.com"));
        assert!(!glob_match_pattern("*@example.com", "user@other.com"));
    }

    #[test]
    fn glob_middle_wildcard() {
        assert!(glob_match_pattern("user_*_admin", "user_123_admin"));
        assert!(!glob_match_pattern("user_*_admin", "user_123_mod"));
    }

    #[test]
    fn glob_star_only_matches_everything() {
        assert!(glob_match_pattern("*", "anything"));
        assert!(glob_match_pattern("*", ""));
    }

    // ── BaseChannel::is_allowed — 精确匹配（原有行为不变）────────────────────

    #[test]
    fn empty_allowlist_permits_all() {
        let ch = BaseChannel::new("test");
        assert!(ch.is_allowed("anyone"));
        assert!(ch.is_allowed("12345"));
    }

    #[test]
    fn exact_id_match() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["123456".into()]);
        assert!(ch.is_allowed("123456"));
        assert!(!ch.is_allowed("999999"));
    }

    #[test]
    fn compound_id_username_format() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["123456".into()]);
        // "123456" 在白名单中，应匹配 "123456|alice"（id 部分）
        assert!(ch.is_allowed("123456|alice"));
    }

    #[test]
    fn at_prefix_stripped() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["@alice".into()]);
        assert!(ch.is_allowed("alice"));
        assert!(ch.is_allowed("123|alice"));
    }

    // ── BaseChannel::is_allowed — Glob 通配符（新行为）──────────────────────

    #[test]
    fn glob_prefix_in_allowlist() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["admin_*".into()]);
        assert!(ch.is_allowed("admin_alice"));
        assert!(ch.is_allowed("admin_bob"));
        assert!(!ch.is_allowed("user_charlie"));
    }

    #[test]
    fn glob_suffix_in_allowlist() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["*@corp.example".into()]);
        assert!(ch.is_allowed("alice@corp.example"));
        assert!(!ch.is_allowed("alice@other.example"));
    }

    #[test]
    fn glob_case_insensitive() {
        // Glob 匹配不区分大小写
        let ch = BaseChannel::new("test").with_allow_from(vec!["Admin_*".into()]);
        assert!(ch.is_allowed("admin_alice")); // 小写 sender，混合大小写 pattern
        assert!(ch.is_allowed("ADMIN_BOB"));  // 大写 sender
    }

    #[test]
    fn glob_matches_username_part_of_compound() {
        // "admin_*" 应通过 username 部分匹配 "99|admin_alice"
        let ch = BaseChannel::new("test").with_allow_from(vec!["admin_*".into()]);
        assert!(ch.is_allowed("99|admin_alice"));
        assert!(!ch.is_allowed("99|user_bob"));
    }

    #[test]
    fn glob_and_exact_entries_coexist() {
        let ch = BaseChannel::new("test")
            .with_allow_from(vec!["123456".into(), "admin_*".into()]);
        assert!(ch.is_allowed("123456"));        // 精确匹配
        assert!(ch.is_allowed("admin_charlie")); // glob 匹配
        assert!(!ch.is_allowed("999|bob"));      // 均不匹配
    }

    // ── is_group_allowed ──────────────────────────────────────────────────────

    #[test]
    fn group_allowlist_falls_back_to_allow_from() {
        let ch = BaseChannel::new("test").with_allow_from(vec!["alice".into()]);
        assert!(ch.is_group_allowed("alice"));
        assert!(!ch.is_group_allowed("bob"));
    }

    #[test]
    fn group_allowlist_overrides_allow_from() {
        let ch = BaseChannel::new("test")
            .with_allow_from(vec!["alice".into()])
            .with_group_config(vec!["group_*".into()], false);
        // group_config 设置后，allow_from 不再用于群组检查
        assert!(!ch.is_group_allowed("alice"));
        assert!(ch.is_group_allowed("group_chat_1"));
    }
}
