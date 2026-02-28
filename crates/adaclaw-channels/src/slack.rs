//! Slack 渠道 — Events API + Webhook 模式
//!
//! # 工作原理
//!
//! 1. Slack 将消息事件以 HTTP POST 发送到本渠道 Webhook 地址
//! 2. 本渠道验证 HMAC-SHA256 签名（X-Slack-Signature），解析消息
//! 3. `send()` 通过 Slack `chat.postMessage` API 回复
//!
//! # 修复
//!
//! - **mrkdwn 格式转换**：Slack 使用 mrkdwn（`*bold*`、`_italic_`），
//!   不是 Markdown（`**bold**`、`_italic_`）；发送前自动转换
//! - **Thread 支持**：session_id 携带 `channel_id/thread_ts` 格式时，
//!   回复自动使用 `thread_ts` 发送到对应 thread
//!
//! # 配置示例
//!
//! ```toml
//! [channels.slack]
//! kind = "slack"
//! token = "xoxb-..."              # Slack Bot Token
//! webhook_secret = "..."          # Signing Secret（Slack App → Basic Information）
//! allow_from = []
//!
//! [channels.slack.extra]
//! webhook_port = "9004"
//! webhook_path = "/webhook/slack"
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

const SLACK_API: &str = "https://slack.com/api";

// ── Slack 请求体 ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SlackPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    challenge: Option<String>,
    event: Option<SlackEvent>,
    team_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct SlackEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    user: Option<String>,
    text: Option<String>,
    channel: Option<String>,
    ts: Option<String>,
    /// thread_ts：若消息在 thread 中，此字段为 thread 根消息的 ts
    thread_ts: Option<String>,
    bot_id: Option<String>,
    subtype: Option<String>,
}

// ── 共享状态 ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct SlackState {
    base: Arc<BaseChannel>,
    bot_token: String,
    signing_secret: Option<String>,
    bus: Arc<dyn MessageBus>,
    http_client: reqwest::Client,
}

impl SlackState {
    /// 验证 Slack 签名
    /// v0={HMAC-SHA256("v0:{timestamp}:{body}", signing_secret)}
    fn verify_signature(
        &self,
        headers: &HeaderMap,
        body: &str,
    ) -> bool {
        let secret = match &self.signing_secret {
            Some(s) => s,
            None => return true, // 未配置密钥，跳过验证
        };

        let timestamp = headers
            .get("X-Slack-Request-Timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let received_sig = headers
            .get("X-Slack-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if timestamp.is_empty() || received_sig.is_empty() {
            return false;
        }

        // 防重放：时间戳与当前时间差不超过 5 分钟
        if let Ok(ts) = timestamp.parse::<i64>() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if (now - ts).abs() > 300 {
                warn!(channel = "slack", "Request timestamp too old, possible replay attack");
                return false;
            }
        }

        let message = format!("v0:{}:{}", timestamp, body);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(message.as_bytes());
        let result = mac.finalize().into_bytes();
        let expected = format!("v0={}", hex::encode(result));

        expected == received_sig
    }

    /// 发送消息（支持 thread）
    ///
    /// - `channel_id`：Slack channel ID
    /// - `text`：消息内容（Markdown 会被转为 mrkdwn）
    /// - `thread_ts`：可选，若提供则回复到该 thread
    async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/chat.postMessage", SLACK_API);
        // 转换 Markdown → Slack mrkdwn 格式
        let mrkdwn_text = markdown_to_slack_mrkdwn(text);

        let mut body = json!({
            "channel": channel_id,
            "text": mrkdwn_text,
            "mrkdwn": true,
        });
        // 如有 thread_ts，回复到该 thread
        if let Some(ts) = thread_ts {
            body["thread_ts"] = json!(ts);
        }

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("Slack postMessage request failed: {}", e))?;

        let resp_body: Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("Slack postMessage response parse failed: {}", e))?;

        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(anyhow!("Slack postMessage failed: {}", err));
        }

        Ok(())
    }
}

// ── SlackChannel ──────────────────────────────────────────────────────────────

pub struct SlackChannel {
    base: Arc<BaseChannel>,
    bot_token: String,
    signing_secret: Option<String>,
    webhook_port: u16,
    webhook_path: String,
    http_client: reqwest::Client,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl SlackChannel {
    pub fn new(
        bot_token: String,
        signing_secret: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
    ) -> Self {
        let base = Arc::new(BaseChannel::new("slack").with_allow_from(allow_from));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            bot_token,
            signing_secret,
            webhook_port,
            webhook_path,
            http_client: client,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = SlackState {
            base: Arc::clone(&self.base),
            bot_token: self.bot_token.clone(),
            signing_secret: self.signing_secret.clone(),
            bus,
            http_client: self.http_client.clone(),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, post(handle_slack_event))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "slack",
            addr = %addr,
            path = %self.webhook_path,
            "Slack Events API webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async { rx.await.ok(); })
            .await
            .map_err(|e| anyhow!("Slack HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "slack", "Slack channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let session_id = &msg.target_session_id;
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        // 解析 session_id："channel_id" 或 "channel_id/thread_ts"
        let (channel_id, thread_ts) = parse_slack_session_id(session_id);

        let state = SlackState {
            base: Arc::clone(&self.base),
            bot_token: self.bot_token.clone(),
            signing_secret: self.signing_secret.clone(),
            bus: Arc::new(crate::DummyBus),
            http_client: self.http_client.clone(),
        };
        state.post_message(&channel_id, &content, thread_ts.as_deref()).await
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.base.set_running(false);
        Ok(())
    }
}

// ── Axum Handler ──────────────────────────────────────────────────────────────

async fn handle_slack_event(
    State(state): State<SlackState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    // 签名验证
    if !state.verify_signature(&headers, &body) {
        warn!(channel = "slack", "Signature verification failed");
        return (StatusCode::FORBIDDEN, Json(json!({})));
    }

    let payload: SlackPayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(channel = "slack", error = %e, "Failed to parse Slack payload");
            return (StatusCode::BAD_REQUEST, Json(json!({})));
        }
    };

    // URL 验证挑战
    if payload.event_type.as_deref() == Some("url_verification") {
        if let Some(challenge) = &payload.challenge {
            return (StatusCode::OK, Json(json!({ "challenge": challenge })));
        }
    }

    // 处理消息事件
    if payload.event_type.as_deref() == Some("event_callback") {
        if let Some(event) = &payload.event {
            if event.event_type.as_deref() == Some("message") {
                // 忽略 Bot 消息和特殊子类型
                if event.bot_id.is_some() || event.subtype.is_some() {
                    return (StatusCode::OK, Json(json!({})));
                }

                let sender_id = event.user.clone().unwrap_or_default();
                let channel_id = event.channel.clone().unwrap_or_default();
                let text = event.text.clone().unwrap_or_default();
                let ts = event.ts.clone().unwrap_or_default();

                if sender_id.is_empty() || channel_id.is_empty() || text.trim().is_empty() {
                    return (StatusCode::OK, Json(json!({})));
                }

                debug!(
                    channel = "slack",
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    preview = %text.chars().take(60).collect::<String>(),
                    "Slack message received"
                );

                // thread_ts：若消息在 thread 中则有值，否则用 ts 自身
                // session_id 格式："channel_id" 或 "channel_id/thread_ts"
                // send() 会解析此格式，将回复发到正确的 thread
                let thread_ts = event.thread_ts.clone().or_else(|| Some(ts.clone()));
                let session_id = if let Some(ref tts) = thread_ts {
                    if tts == &ts {
                        // 顶层消息，session_id = channel_id/ts（创建新 thread）
                        format!("{}/{}", channel_id, ts)
                    } else {
                        // 在已有 thread 中回复
                        format!("{}/{}", channel_id, tts)
                    }
                } else {
                    channel_id.clone()
                };

                let mut metadata: HashMap<String, Value> = HashMap::new();
                metadata.insert("ts".to_string(), Value::String(ts.clone()));
                if let Some(ref tts) = event.thread_ts {
                    metadata.insert("thread_ts".to_string(), Value::String(tts.clone()));
                }
                metadata.insert(
                    "team_id".to_string(),
                    Value::String(payload.team_id.clone().unwrap_or_default()),
                );

                // session_id = "channel_id/thread_ts"（send() 方法回复到对应 thread）
                state
                    .base
                    .handle_message(
                        &state.bus,
                        &sender_id,
                        &sender_id,
                        &session_id,
                        text.trim(),
                        metadata,
                    )
                    .await;
            }
        }
    }

    (StatusCode::OK, Json(json!({})))
}

// ── Markdown → Slack mrkdwn 转换 ──────────────────────────────────────────────
//
// Slack 使用 mrkdwn 格式，与 Markdown 不同：
// - 粗体：`*text*`（不是 `**text**`）
// - 斜体：`_text_`（相同）
// - 删除线：`~text~`（不是 `~~text~~`）
// - 行内代码：` `code` `（相同）
// - 代码块：` ```code``` `（相同但不支持语言标注）
// - 链接：`<url|text>`（不是 `[text](url)`）
// - 无序列表：`•`（保留）
// - 标题：无，改为粗体

pub fn markdown_to_slack_mrkdwn(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // Step 1: 提取并保护代码块（```...```）
    let mut code_blocks: Vec<String> = Vec::new();
    let re_code_block = regex::Regex::new(r"(?s)```[\w]*\n?(.*?)```").unwrap();
    let mut result = re_code_block
        .replace_all(text, |caps: &regex::Captures| {
            let idx = code_blocks.len();
            code_blocks.push(caps[1].to_string());
            format!("\x00CB{}\x00", idx)
        })
        .into_owned();

    // Step 2: 提取并保护行内代码（`...`）
    let mut inline_codes: Vec<String> = Vec::new();
    let re_inline_code = regex::Regex::new(r"`([^`]+)`").unwrap();
    result = re_inline_code
        .replace_all(&result, |caps: &regex::Captures| {
            let idx = inline_codes.len();
            inline_codes.push(caps[1].to_string());
            format!("\x00IC{}\x00", idx)
        })
        .into_owned();

    // Step 3: Markdown 链接 [text](url) → Slack <url|text>
    let re_link = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = re_link
        .replace_all(&result, |caps: &regex::Captures| {
            format!("<{}|{}>", &caps[2], &caps[1])
        })
        .into_owned();

    // Step 4: 粗体 **text** / __text__ → *text*
    let re_bold_star = regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();
    result = re_bold_star.replace_all(&result, "*$1*").into_owned();
    let re_bold_under = regex::Regex::new(r"__(.+?)__").unwrap();
    result = re_bold_under.replace_all(&result, "*$1*").into_owned();

    // Step 5: 删除线 ~~text~~ → ~text~
    let re_strike = regex::Regex::new(r"~~(.+?)~~").unwrap();
    result = re_strike.replace_all(&result, "~$1~").into_owned();

    // Step 6: 标题 # Title → *Title*（mrkdwn 无标题，用粗体代替）
    let re_heading = regex::Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    result = re_heading.replace_all(&result, "*$1*").into_owned();

    // Step 7: 移除 > 引用标记
    let re_blockquote = regex::Regex::new(r"(?m)^>\s*(.*)$").unwrap();
    result = re_blockquote.replace_all(&result, "$1").into_owned();

    // Step 8: 无序列表 - item / * item → • item
    let re_list = regex::Regex::new(r"(?m)^[-*]\s+").unwrap();
    result = re_list.replace_all(&result, "• ").into_owned();

    // Step 9: 还原行内代码
    for (i, code) in inline_codes.iter().enumerate() {
        result = result.replace(
            &format!("\x00IC{}\x00", i),
            &format!("`{}`", code),
        );
    }

    // Step 10: 还原代码块
    for (i, code) in code_blocks.iter().enumerate() {
        result = result.replace(
            &format!("\x00CB{}\x00", i),
            &format!("```{}```", code),
        );
    }

    result
}

// ── Session ID 解析 ───────────────────────────────────────────────────────────

/// 解析 Slack session_id 格式
///
/// 格式：
/// - `"C1234567890"` → `("C1234567890", None)` （直接回复到 channel）
/// - `"C1234567890/1234567890.123456"` → `("C1234567890", Some("1234567890.123456"))` （回复到 thread）
pub fn parse_slack_session_id(session_id: &str) -> (String, Option<String>) {
    if let Some(slash_pos) = session_id.rfind('/') {
        let channel = &session_id[..slash_pos];
        let thread_ts = &session_id[slash_pos + 1..];
        if !channel.is_empty() && !thread_ts.is_empty() {
            return (channel.to_string(), Some(thread_ts.to_string()));
        }
    }
    (session_id.to_string(), None)
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mrkdwn_bold() {
        let result = markdown_to_slack_mrkdwn("Hello **world**!");
        assert_eq!(result, "Hello *world*!");
    }

    #[test]
    fn test_mrkdwn_link() {
        let result = markdown_to_slack_mrkdwn("[Google](https://google.com)");
        assert_eq!(result, "<https://google.com|Google>");
    }

    #[test]
    fn test_mrkdwn_strikethrough() {
        let result = markdown_to_slack_mrkdwn("~~deleted~~");
        assert_eq!(result, "~deleted~");
    }

    #[test]
    fn test_mrkdwn_heading() {
        let result = markdown_to_slack_mrkdwn("# Title\nSome text");
        assert!(result.contains("*Title*"));
    }

    #[test]
    fn test_mrkdwn_code_block_preserved() {
        let input = "```rust\nfn main() {}\n```";
        let result = markdown_to_slack_mrkdwn(input);
        assert!(result.contains("fn main() {}"));
        assert!(result.contains("```"));
    }

    #[test]
    fn test_parse_slack_session_id_simple() {
        let (ch, ts) = parse_slack_session_id("C1234567890");
        assert_eq!(ch, "C1234567890");
        assert!(ts.is_none());
    }

    #[test]
    fn test_parse_slack_session_id_with_thread() {
        let (ch, ts) = parse_slack_session_id("C1234567890/1234567890.123456");
        assert_eq!(ch, "C1234567890");
        assert_eq!(ts.as_deref(), Some("1234567890.123456"));
    }
}
