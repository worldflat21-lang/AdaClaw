//! `BaseChannel` — 所有渠道共享的辅助结构体
//!
//! 提供：
//! - `is_allowed()` — 白名单检查（支持 "id|username" 复合格式，参考 picoclaw）
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
        // 拆解 "id|username" 格式
        let (id_part, user_part) = if let Some(idx) = sender_id.find('|') {
            (&sender_id[..idx], &sender_id[idx + 1..])
        } else {
            (sender_id, "")
        };

        for allowed in list {
            // 去掉 "@" 前缀（允许配置 "@alice"）
            let trimmed = allowed.trim_start_matches('@');

            if sender_id == allowed
                || sender_id == trimmed
                || id_part == allowed
                || id_part == trimmed
                || (!user_part.is_empty() && (user_part == allowed || user_part == trimmed))
            {
                return true;
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
