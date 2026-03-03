use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait MessageBus: Send + Sync {
    async fn send_inbound(&self, msg: InboundMessage) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Image(Vec<u8>),
    Audio(Vec<u8>),
    File { name: String, data: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: Uuid,
    pub channel: String,
    pub session_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub content: MessageContent,
    pub reply_to: Option<Uuid>,
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: Uuid,
    pub target_channel: String,
    pub target_session_id: String,
    pub content: MessageContent,
    pub reply_to: Option<Uuid>,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    /// 默认返回 false；实现类通过 BaseChannel::is_running() 覆盖
    fn is_running(&self) -> bool {
        false
    }

    /// Send an approval prompt with Approve/Deny interactive UI to a session.
    ///
    /// Channels that support interactive approval (e.g. Telegram inline keyboard)
    /// should override this method. The default implementation is a no-op.
    ///
    /// # Arguments
    /// - `session_id` — target chat/channel ID to send the prompt to
    /// - `request_id` — pending approval request ID (e.g. `"apr-abc12"`)
    /// - `tool_name` — name of the tool awaiting approval
    /// - `args_preview` — short preview of the tool's arguments (≤200 chars)
    async fn send_approval_prompt(
        &self,
        session_id: &str,
        request_id: &str,
        tool_name: &str,
        args_preview: &str,
    ) -> Result<()> {
        let _ = (session_id, request_id, tool_name, args_preview);
        Ok(())
    }

    /// Whether this channel supports interactive approval prompts (e.g. inline buttons).
    ///
    /// Returns `true` for channels that implement `send_approval_prompt()` meaningfully.
    fn supports_approval_prompts(&self) -> bool {
        false
    }

    // ── 草稿/流式输出（三阶段模型，来自 zeroclaw）──────────────────────────────

    /// 此渠道是否支持草稿消息的就地编辑（流式输出所必需）。
    ///
    /// 返回 `true` 的渠道应实现 `send_draft / update_draft / finalize_draft`。
    /// 默认返回 `false`；Telegram 已在业务侧实现 placeholder 编辑，
    /// 后续可通过覆盖此方法正式接入草稿管线。
    fn supports_draft_updates(&self) -> bool {
        false
    }

    /// 发送一条草稿消息（"Thinking…" 占位），返回平台消息 ID 用于后续编辑。
    ///
    /// 返回 `Ok(None)` 表示该渠道不支持草稿（降级为普通发送）。
    /// 默认实现为 no-op，现有渠道无需修改。
    ///
    /// # Arguments
    /// - `session_id` — 目标会话 ID（与 `OutboundMessage::target_session_id` 一致）
    /// - `text`       — 初始占位文本（如 `"Thinking… 💭"`）
    async fn send_draft(&self, session_id: &str, text: &str) -> Result<Option<String>> {
        let _ = (session_id, text);
        Ok(None)
    }

    /// 就地编辑草稿，追加/替换新的文本内容。
    ///
    /// 返回 `Ok(None)` 表示继续使用当前 `draft_id`；
    /// 返回 `Ok(Some(new_id))` 表示平台创建了新的续发消息（如触发编辑次数上限）。
    /// 默认实现为 no-op。
    ///
    /// # Arguments
    /// - `session_id` — 目标会话 ID
    /// - `draft_id`   — 上一步返回的草稿消息 ID
    /// - `text`       — 当前累积的完整文本
    async fn update_draft(
        &self,
        session_id: &str,
        draft_id: &str,
        text: &str,
    ) -> Result<Option<String>> {
        let _ = (session_id, draft_id, text);
        Ok(None)
    }

    /// 将草稿最终确认为完整回复（可在此时应用 Markdown/HTML 格式化）。
    ///
    /// 默认实现为 no-op。
    ///
    /// # Arguments
    /// - `session_id` — 目标会话 ID
    /// - `draft_id`   — 草稿消息 ID
    /// - `text`       — 最终完整文本
    async fn finalize_draft(&self, session_id: &str, draft_id: &str, text: &str) -> Result<()> {
        let _ = (session_id, draft_id, text);
        Ok(())
    }

    /// 取消并删除草稿消息（如发生错误需回滚时调用）。
    ///
    /// 默认实现为 no-op。
    async fn cancel_draft(&self, session_id: &str, draft_id: &str) -> Result<()> {
        let _ = (session_id, draft_id);
        Ok(())
    }

    // ── Emoji Reaction（来自 moltis + zeroclaw 共同验证）──────────────────────

    /// 对指定消息添加 emoji reaction（如 "👀" 表示正在处理）。
    ///
    /// 两个独立项目（moltis `ChannelOutbound` 和 zeroclaw `Channel`）均实现了相同的
    /// no-op 默认方法，说明这是渠道 trait 的标准扩展点。
    /// Discord / Slack 等支持 reaction 的渠道可覆盖此方法实现真实逻辑。
    ///
    /// # Arguments
    /// - `session_id` — 平台渠道/会话标识符（如 Discord channel ID、Slack channel ID）
    /// - `message_id` — 平台消息 ID
    /// - `emoji`      — Unicode emoji（如 `"👀"`、`"✅"`、`"⚙️"`）
    async fn add_reaction(
        &self,
        session_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let _ = (session_id, message_id, emoji);
        Ok(())
    }

    /// 移除之前由本 bot 添加的 emoji reaction。
    ///
    /// 默认实现为 no-op。
    async fn remove_reaction(
        &self,
        session_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let _ = (session_id, message_id, emoji);
        Ok(())
    }
}


