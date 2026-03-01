//! `adaclaw-channels` — 多渠道接入实现
//!
//! # 渠道列表
//!
//! | 渠道           | 模块          | 接入方式              |
//! |--------------|---------------|--------------------|
//! | CLI          | `cli`         | stdin/stdout REPL   |
//! | Telegram     | `telegram`    | 长轮询（Bot API）    |
//! | 钉钉          | `dingtalk`    | Outgoing Webhook    |
//! | 飞书/Lark     | `feishu`      | 事件订阅 Webhook     |
//! | 企业微信       | `wechat_work` | AIBot Webhook + AES-CBC |
//! | Discord      | `discord`     | Gateway WebSocket   |
//! | Slack        | `slack`       | Events API Webhook  |
//! | 通用 Webhook  | `webhook`     | HTTP POST           |
//! | WhatsApp     | `whatsapp`    | Business Cloud API  |
//! | Email        | `email`       | IMAP + SMTP         |
//! | Matrix       | `matrix`      | Client-Server API（`feature = "matrix"`） |

/// Phase 14-P0-2: structured error types for library crate consumers.
pub mod error;
pub mod base;
pub mod cli;
pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod manager;
pub mod slack;
pub mod telegram;
pub mod wechat_work;
pub mod webhook;
pub mod whatsapp;

#[cfg(feature = "matrix")]
pub mod matrix;

pub use base::BaseChannel;
pub use cli::CliChannel;
pub use dingtalk::DingTalkChannel;
pub use discord::DiscordChannel;
pub use email::EmailChannel;
pub use feishu::FeishuChannel;
pub use manager::ChannelManager;
pub use slack::SlackChannel;
pub use telegram::{markdown_to_telegram_html, TelegramChannel};
pub use wechat_work::WeComChannel;
pub use webhook::WebhookChannel;
pub use whatsapp::WhatsAppChannel;

#[cfg(feature = "matrix")]
pub use matrix::MatrixChannel;

// ── DummyBus ──────────────────────────────────────────────────────────────────
//
// 供 `send()` 方法内部使用：在构建需要 `Arc<dyn MessageBus>` 的状态结构体时，
// 作为占位符传入（send() 中实际不会调用 send_inbound，因此是安全的）。

use adaclaw_core::channel::{InboundMessage, MessageBus};
use anyhow::Result;
use async_trait::async_trait;

/// 空实现的 MessageBus，用作内部占位符（丢弃所有入站消息）。
pub(crate) struct DummyBus;

#[async_trait]
impl MessageBus for DummyBus {
    async fn send_inbound(&self, _msg: InboundMessage) -> Result<()> {
        Ok(())
    }
}
