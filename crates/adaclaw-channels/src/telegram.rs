//! Telegram Bot API 渠道实现（长轮询模式）
//!
//! # 设计亮点（借鉴 nanobot + picoclaw）
//!
//! - **"Thinking..." 占位消息**：收到用户消息后立即发一条 "Thinking... 💭"，
//!   Agent 回复完成后用 `editMessageText` 编辑它，比 typing indicator 体验更好。
//! - **Markdown → Telegram HTML** 转换：支持粗体/斜体/代码/链接/删除线。
//! - **id|username 复合白名单**：支持同时按 Telegram user_id 和 username 过滤。
//! - **消息分片**：超过 4000 字符时自动按换行符分片发送。
//! - **Webhook HMAC 验证**：`verify_webhook_signature()` 保留向后兼容。

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ── Telegram API 响应结构 ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgMessage {
    message_id: i64,
    from: Option<TgUser>,
    chat: TgChat,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<TgPhotoSize>>,
    voice: Option<TgFile>,
    audio: Option<TgFile>,
    document: Option<TgFile>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgUser {
    id: i64,
    username: Option<String>,
    first_name: String,
}

#[derive(Debug, Deserialize, Clone)]
struct TgChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

#[derive(Debug, Deserialize, Clone)]
struct TgPhotoSize {
    file_id: String,
}

#[derive(Debug, Deserialize, Clone)]
struct TgFile {
    file_id: String,
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgSentMessage {
    message_id: i64,
}

// ── TelegramChannel ───────────────────────────────────────────────────────────

pub struct TelegramChannel {
    base: BaseChannel,
    token: String,
    client: reqwest::Client,
    /// chat_id → 占位消息 message_id（"Thinking... 💭"）
    placeholders: Arc<Mutex<HashMap<String, i64>>>,
    /// 机器人代理（可选）
    proxy: Option<String>,
}

impl TelegramChannel {
    pub fn new(token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            base: BaseChannel::new("telegram"),
            token,
            client,
            placeholders: Arc::new(Mutex::new(HashMap::new())),
            proxy: None,
        }
    }

    pub fn with_allow_from(mut self, allow_from: Vec<String>) -> Self {
        self.base = self.base.with_allow_from(allow_from);
        self
    }

    pub fn with_group_config(mut self, groups: Vec<String>, require_mention: bool) -> Self {
        self.base = self.base.with_group_config(groups, require_mention);
        self
    }

    pub fn with_proxy(mut self, proxy: String) -> Self {
        self.proxy = Some(proxy);
        self
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    // ── Bot API 调用 ──────────────────────────────────────────────────────────

    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let url = self.api_url(method);
        let resp = self
            .client
            .post(&url)
            .json(&params)
            .send()
            .await
            .map_err(|e| anyhow!("Telegram API request failed ({}): {}", method, e))?;

        let tg: TgResponse<T> = resp
            .json()
            .await
            .map_err(|e| anyhow!("Telegram API JSON parse failed ({}): {}", method, e))?;

        if !tg.ok {
            return Err(anyhow!(
                "Telegram API error ({}): {}",
                method,
                tg.description.unwrap_or_default()
            ));
        }

        tg.result.ok_or_else(|| anyhow!("Telegram API returned null result ({})", method))
    }

    /// 轮询新消息（timeout=30s 长轮询）
    async fn get_updates(&self, offset: i64) -> Result<Vec<TgUpdate>> {
        let url = self.api_url("getUpdates");
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", "30".to_string()),
                ("limit", "100".to_string()),
                ("allowed_updates", r#"["message"]"#.to_string()),
            ])
            .send()
            .await
            .map_err(|e| anyhow!("getUpdates request failed: {}", e))?;

        let tg: TgResponse<Vec<TgUpdate>> = resp
            .json()
            .await
            .map_err(|e| anyhow!("getUpdates JSON parse failed: {}", e))?;

        Ok(tg.result.unwrap_or_default())
    }

    /// 发送文本消息（HTML 格式），返回 message_id
    async fn send_message(&self, chat_id: i64, text: &str, parse_html: bool) -> Result<i64> {
        let mut params = json!({
            "chat_id": chat_id,
            "text": text,
        });
        if parse_html {
            params["parse_mode"] = json!("HTML");
        }
        let msg: TgSentMessage = self.call_api("sendMessage", params).await?;
        Ok(msg.message_id)
    }

    /// 编辑已存在的消息
    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<()> {
        let html = markdown_to_telegram_html(text);
        let result = self
            .call_api::<Value>(
                "editMessageText",
                json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "text": html,
                    "parse_mode": "HTML",
                }),
            )
            .await;

        if let Err(e) = result {
            // 编辑失败时降级到新消息
            warn!("editMessageText failed ({}), falling back to sendMessage", e);
            let plain = self.send_message(chat_id, text, false).await;
            if let Err(e2) = plain {
                error!("fallback sendMessage also failed: {}", e2);
            }
        }
        Ok(())
    }

    /// 发送 typing... 动作
    async fn send_typing(&self, chat_id: i64) {
        let _ = self
            .call_api::<Value>(
                "sendChatAction",
                json!({ "chat_id": chat_id, "action": "typing" }),
            )
            .await;
    }

    // ── 消息处理 ──────────────────────────────────────────────────────────────

    async fn process_update(
        &self,
        update: &TgUpdate,
        bus: &Arc<dyn MessageBus>,
    ) {
        let msg = match &update.message {
            Some(m) => m,
            None => return,
        };

        let user = match &msg.from {
            Some(u) => u,
            None => return,
        };

        // 构建 sender_id（复合格式 "id|username"，便于白名单匹配）
        let sender_id = if let Some(uname) = &user.username {
            format!("{}|{}", user.id, uname)
        } else {
            user.id.to_string()
        };
        let sender_name = user.first_name.clone();
        let chat_id = msg.chat.id;
        let chat_id_str = chat_id.to_string();
        let is_group = msg.chat.chat_type != "private";

        // 白名单检查
        let allowed = if is_group {
            self.base.is_group_allowed(&sender_id)
        } else {
            self.base.is_allowed(&sender_id)
        };
        if !allowed {
            warn!(
                sender_id = %sender_id,
                channel = "telegram",
                "Access denied: sender not in allowlist"
            );
            return;
        }

        // 提取文本内容
        let mut content_parts: Vec<String> = Vec::new();
        if let Some(text) = &msg.text {
            content_parts.push(text.clone());
        }
        if let Some(caption) = &msg.caption {
            content_parts.push(caption.clone());
        }

        // 处理媒体（标注类型，后续可接语音转写）
        let mut metadata_extra: HashMap<String, Value> = HashMap::new();
        if let Some(photos) = &msg.photo {
            if let Some(largest) = photos.last() {
                content_parts.push("[image]".to_string());
                metadata_extra.insert(
                    "photo_file_id".to_string(),
                    Value::String(largest.file_id.clone()),
                );
            }
        }
        if let Some(voice) = &msg.voice {
            content_parts.push("[voice]".to_string());
            metadata_extra.insert(
                "voice_file_id".to_string(),
                Value::String(voice.file_id.clone()),
            );
        }
        if let Some(audio) = &msg.audio {
            content_parts.push("[audio]".to_string());
            metadata_extra.insert(
                "audio_file_id".to_string(),
                Value::String(audio.file_id.clone()),
            );
        }
        if let Some(doc) = &msg.document {
            content_parts.push("[file]".to_string());
            metadata_extra.insert(
                "document_file_id".to_string(),
                Value::String(doc.file_id.clone()),
            );
        }

        let content = if content_parts.is_empty() {
            "[empty message]".to_string()
        } else {
            content_parts.join("\n")
        };

        debug!(
            chat_id = %chat_id,
            sender_id = %sender_id,
            preview = %&content.chars().take(60).collect::<String>(),
            "Telegram message received"
        );

        // 发送 typing 动作
        self.send_typing(chat_id).await;

        // 发送 "Thinking..." 占位消息
        match self
            .send_message(chat_id, "Thinking... 💭", false)
            .await
        {
            Ok(mid) => {
                let mut pmap = self.placeholders.lock().unwrap();
                pmap.insert(chat_id_str.clone(), mid);
            }
            Err(e) => {
                warn!(error = %e, "Failed to send Thinking placeholder");
            }
        }

        // 构建 metadata
        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert(
            "message_id".to_string(),
            Value::String(msg.message_id.to_string()),
        );
        metadata.insert(
            "user_id".to_string(),
            Value::String(user.id.to_string()),
        );
        metadata.insert(
            "first_name".to_string(),
            Value::String(user.first_name.clone()),
        );
        metadata.insert(
            "is_group".to_string(),
            Value::Bool(is_group),
        );
        for (k, v) in metadata_extra {
            metadata.insert(k, v);
        }

        // 上报到 Bus（session_id = chat_id，供出站路由使用）
        self.base
            .handle_message(
                bus,
                &sender_id,
                &sender_name,
                &chat_id_str,
                &content,
                metadata,
            )
            .await;
    }

    /// 将回复内容分片发送（每片最多 4000 字符）
    async fn send_reply(&self, chat_id: i64, chat_id_str: &str, content: &str) {
        let chunks = split_message(content, 4000);

        // 第一片：尝试编辑占位消息
        let mut first_chunk = true;
        for chunk in &chunks {
            if first_chunk {
                first_chunk = false;
                let placeholder_id = {
                    let mut pmap = self.placeholders.lock().unwrap();
                    pmap.remove(chat_id_str)
                };
                if let Some(mid) = placeholder_id {
                    if let Err(e) = self.edit_message_text(chat_id, mid, chunk).await {
                        error!("edit placeholder failed: {}", e);
                        // 编辑失败，降级到新消息
                        let html = markdown_to_telegram_html(chunk);
                        if let Err(e2) = self.send_message(chat_id, &html, true).await {
                            error!("send new message also failed: {}", e2);
                        }
                    }
                    continue;
                }
            }
            // 后续片段或没有占位符，发新消息
            let html = markdown_to_telegram_html(chunk);
            if let Err(e) = self.send_message(chat_id, &html, true).await {
                // 降级到纯文本
                warn!("HTML send failed ({}), trying plain text", e);
                if let Err(e2) = self.send_message(chat_id, chunk, false).await {
                    error!("plain text send also failed: {}", e2);
                }
            }
        }
    }

    /// Webhook HMAC-SHA256 签名验证（供 Webhook 模式使用）
    pub fn verify_webhook_signature(&self, payload: &str, signature: &str) -> bool {
        use hmac::{Hmac, Mac};
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(self.token.as_bytes());
        let secret_key = hasher.finalize();

        let mut mac = match Hmac::<Sha256>::new_from_slice(&secret_key) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(payload.as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());
        expected == signature
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        if self.token.is_empty() {
            return Err(anyhow!("Telegram bot token is not configured"));
        }

        self.base.set_running(true);
        info!("Starting Telegram channel (long polling)...");

        let mut offset: i64 = 0;
        let mut consecutive_errors: u32 = 0;

        while self.base.is_running() {
            match self.get_updates(offset).await {
                Ok(updates) => {
                    consecutive_errors = 0;
                    for update in &updates {
                        if update.update_id >= offset {
                            offset = update.update_id + 1;
                        }
                        self.process_update(update, &bus).await;
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let wait = std::cmp::min(consecutive_errors * 2, 30);
                    warn!(
                        error = %e,
                        retry_in = wait,
                        "getUpdates failed, retrying in {}s", wait
                    );
                    tokio::time::sleep(Duration::from_secs(wait as u64)).await;
                }
            }
        }

        self.base.set_running(false);
        info!("Telegram channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if !self.base.is_running() {
            return Err(anyhow!("Telegram channel is not running"));
        }

        let chat_id: i64 = msg.target_session_id.parse().map_err(|_| {
            anyhow!("Invalid Telegram chat_id: '{}'", msg.target_session_id)
        })?;
        let chat_id_str = msg.target_session_id.clone();

        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => {
                warn!("Telegram channel: non-text outbound message ignored");
                return Ok(());
            }
        };

        self.send_reply(chat_id, &chat_id_str, &content).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        info!("Telegram channel stop requested");
        Ok(())
    }
}

// ── Markdown → Telegram HTML 转换 ─────────────────────────────────────────────
//
// 参考 nanobot (Python) 和 picoclaw (Go) 的实现：
// 1. 保护代码块/行内代码
// 2. 移除 # 标题符号
// 3. 移除 > 引用符号
// 4. HTML 转义
// 5. 链接/粗体/斜体/删除线
// 6. 列表项
// 7. 还原代码块

pub fn markdown_to_telegram_html(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // Step 1: 提取并保护代码块（```...```）
    let mut code_blocks: Vec<String> = Vec::new();
    let re_code_block = Regex::new(r"(?s)```[\w]*\n?(.*?)```").unwrap();
    let mut result = re_code_block
        .replace_all(text, |caps: &regex::Captures| {
            let idx = code_blocks.len();
            code_blocks.push(caps[1].to_string());
            format!("\x00CB{}\x00", idx)
        })
        .into_owned();

    // Step 2: 提取并保护行内代码（`...`）
    let mut inline_codes: Vec<String> = Vec::new();
    let re_inline_code = Regex::new(r"`([^`]+)`").unwrap();
    result = re_inline_code
        .replace_all(&result, |caps: &regex::Captures| {
            let idx = inline_codes.len();
            inline_codes.push(caps[1].to_string());
            format!("\x00IC{}\x00", idx)
        })
        .into_owned();

    // Step 3: 移除 # 标题标记（保留文字）
    let re_heading = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    result = re_heading
        .replace_all(&result, "$1")
        .into_owned();

    // Step 4: 移除 > 引用标记
    let re_blockquote = Regex::new(r"(?m)^>\s*(.*)$").unwrap();
    result = re_blockquote
        .replace_all(&result, "$1")
        .into_owned();

    // Step 5: HTML 转义
    result = result
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    // Step 6: 链接 [text](url)
    let re_link = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = re_link
        .replace_all(&result, r#"<a href="$2">$1</a>"#)
        .into_owned();

    // Step 7: 粗体 **text** / __text__
    let re_bold_star = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    result = re_bold_star.replace_all(&result, "<b>$1</b>").into_owned();
    let re_bold_under = Regex::new(r"__(.+?)__").unwrap();
    result = re_bold_under
        .replace_all(&result, "<b>$1</b>")
        .into_owned();

    // Step 8: 斜体 _text_（避免匹配 some_var_name）
    let re_italic =
        Regex::new(r"(?<![a-zA-Z0-9])_([^_]+)_(?![a-zA-Z0-9])").unwrap();
    result = re_italic.replace_all(&result, "<i>$1</i>").into_owned();

    // Step 9: 删除线 ~~text~~
    let re_strike = Regex::new(r"~~(.+?)~~").unwrap();
    result = re_strike.replace_all(&result, "<s>$1</s>").into_owned();

    // Step 10: 无序列表 - item / * item
    let re_list = Regex::new(r"(?m)^[-*]\s+").unwrap();
    result = re_list.replace_all(&result, "• ").into_owned();

    // Step 11: 还原行内代码
    for (i, code) in inline_codes.iter().enumerate() {
        let escaped = code
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        result = result.replace(
            &format!("\x00IC{}\x00", i),
            &format!("<code>{}</code>", escaped),
        );
    }

    // Step 12: 还原代码块
    for (i, code) in code_blocks.iter().enumerate() {
        let escaped = code
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        result = result.replace(
            &format!("\x00CB{}\x00", i),
            &format!("<pre><code>{}</code></pre>", escaped),
        );
    }

    result
}

/// 将长文本按换行符（优先）或空格分成不超过 max_len 字符的片段。
fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if content.len() <= max_len {
        return vec![content.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = content;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let cut = &remaining[..max_len];
        let pos = cut.rfind('\n').unwrap_or_else(|| {
            cut.rfind(' ').unwrap_or(max_len)
        });
        chunks.push(remaining[..pos].to_string());
        remaining = remaining[pos..].trim_start_matches(|c| c == '\n' || c == ' ');
    }
    chunks
}

// ── 单元测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_bold() {
        let result = markdown_to_telegram_html("Hello **world**!");
        assert!(result.contains("<b>world</b>"));
    }

    #[test]
    fn test_markdown_code_block() {
        let input = "```rust\nfn main() {}\n```";
        let result = markdown_to_telegram_html(input);
        assert!(result.contains("<pre><code>"));
        assert!(result.contains("fn main() {}"));
    }

    #[test]
    fn test_markdown_html_escape() {
        let input = "a < b & c > d";
        let result = markdown_to_telegram_html(input);
        assert!(result.contains("&lt;"));
        assert!(result.contains("&amp;"));
        assert!(result.contains("&gt;"));
    }

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("hello", 4000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn test_split_message_long() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4000);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 4000);
        }
    }

    #[test]
    fn test_allowlist_compound() {
        let ch = TelegramChannel::new("token".to_string())
            .with_allow_from(vec!["123456".to_string()]);
        // compound sender_id "123456|alice" should match allowed "123456"
        assert!(ch.base.is_allowed("123456|alice"));
        // unknown id should be rejected
        assert!(!ch.base.is_allowed("999|alice"));
    }
}
