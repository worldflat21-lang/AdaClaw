//! Telegram Bot API 渠道实现（长轮询模式）
//!
//! # 修复清单
//!
//! - **消息分片上限**：4096 字符（Telegram API 实际限制），按 Unicode 字符数计算
//! - **持续 Typing 循环**：每 4 秒刷新 typing 状态，直到回复发出；避免 5s 过期
//! - **群组 mention-only 模式**：`mention_only = true` 时仅响应 @提及机器人的消息
//! - **Bot 命令**：支持 /start 和 /help 命令
//! - **启动 409 冲突探针**：启动时先用 timeout=0 探测，避免与上一个实例冲突
//! - **"Thinking..." 占位消息**：收到用户消息后立即发占位，回复后编辑替换
//! - **Markdown → Telegram HTML** 转换

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Telegram 消息最大字符数（Unicode code points，非字节数）
const TG_MAX_MESSAGE_CHARS: usize = 4096;

/// Approval callback data prefix — Approve button
const APPROVAL_CALLBACK_APPROVE_PREFIX: &str = "acadapr:yes:";
/// Approval callback data prefix — Deny button
const APPROVAL_CALLBACK_DENY_PREFIX: &str = "acadapr:no:";

// ── Telegram API 响应结构 ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
    callback_query: Option<TgCallbackQuery>,
}

/// Telegram callback_query — sent when the user presses an inline keyboard button.
#[derive(Debug, Deserialize, Clone)]
struct TgCallbackQuery {
    /// Callback query ID (needed for `answerCallbackQuery`).
    id: String,
    /// The user who pressed the button.
    from: TgUser,
    /// The message the inline keyboard was attached to (if still accessible).
    message: Option<TgCallbackMessage>,
    /// The `callback_data` value on the pressed button.
    data: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgCallbackMessage {
    message_id: i64,
    chat: TgChat,
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
    #[allow(dead_code)]
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct TgSentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TgBotInfo {
    username: String,
}

// ── TelegramChannel ───────────────────────────────────────────────────────────

pub struct TelegramChannel {
    base: BaseChannel,
    token: String,
    client: reqwest::Client,
    /// chat_id → 占位消息 message_id（"Thinking... 💭"）
    placeholders: Arc<Mutex<HashMap<String, i64>>>,
    /// chat_id → typing loop task handle
    typing_tasks: Arc<tokio::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// 是否在群组中只响应 @提及的消息
    mention_only: bool,
    /// 机器人用户名（用于 mention-only 模式的 @检测），启动时自动获取
    bot_username: Arc<tokio::sync::Mutex<Option<String>>>,
    /// 机器人代理（可选）
    proxy: Option<String>,
}

impl TelegramChannel {
    pub fn new(token: String) -> Self {
        // reqwest::Client::builder().build() only fails when custom TLS/proxy
        // configuration is invalid.  With a simple timeout, it is effectively
        // infallible; fall back to the default client if it somehow does fail.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base: BaseChannel::new("telegram"),
            token,
            client,
            placeholders: Arc::new(Mutex::new(HashMap::new())),
            typing_tasks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            mention_only: false,
            bot_username: Arc::new(tokio::sync::Mutex::new(None)),
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

    pub fn with_mention_only(mut self, mention_only: bool) -> Self {
        self.mention_only = mention_only;
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
                "Telegram API error ({}): {} [code={}]",
                method,
                tg.description.unwrap_or_default(),
                tg.error_code.unwrap_or(0)
            ));
        }

        tg.result
            .ok_or_else(|| anyhow!("Telegram API returned null result ({})", method))
    }

    /// 获取机器人用户名（用于 mention-only 检测），带缓存
    async fn get_bot_username(&self) -> Option<String> {
        {
            let cache = self.bot_username.lock().await;
            if let Some(ref uname) = *cache {
                return Some(uname.clone());
            }
        }

        let url = self.api_url("getMe");
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(tg) = resp.json::<TgResponse<TgBotInfo>>().await
                    && tg.ok
                    && let Some(info) = tg.result
                {
                    let uname = info.username.clone();
                    let mut cache = self.bot_username.lock().await;
                    *cache = Some(uname.clone());
                    debug!(channel = "telegram", username = %uname, "Bot username fetched");
                    return Some(uname);
                }
                warn!(channel = "telegram", "Failed to parse getMe response");
                None
            }
            Err(e) => {
                warn!(channel = "telegram", error = %e, "getMe request failed");
                None
            }
        }
    }

    /// 启动探针：用 timeout=0 探测，直到成功获得 getUpdates slot
    ///
    /// 如果上一个实例还在 long polling，直接进入 30s 长轮询会得到 409 Conflict。
    /// 用 timeout=0 先快速轮询，直到成功（非 409）后再进入正常循环。
    async fn startup_probe(&self) -> i64 {
        let url = self.api_url("getUpdates");
        let mut offset: i64 = 0;

        loop {
            let body = json!({
                "offset": offset,
                "timeout": 0,
                "allowed_updates": ["message", "callback_query"],
            });

            match self.client.post(&url).json(&body).send().await {
                Err(e) => {
                    warn!(channel = "telegram", error = %e, "Startup probe request failed, retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                Ok(resp) => {
                    let data: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(channel = "telegram", error = %e, "Startup probe parse error, retrying in 5s");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    };

                    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok {
                        // 消费掉已有的 updates，防止重放
                        if let Some(results) = data.get("result").and_then(|r| r.as_array()) {
                            for update in results {
                                if let Some(uid) = update.get("update_id").and_then(|v| v.as_i64())
                                {
                                    offset = uid + 1;
                                }
                            }
                        }
                        debug!(
                            channel = "telegram",
                            offset = offset,
                            "Startup probe succeeded"
                        );
                        return offset;
                    }

                    let error_code = data.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0);

                    if error_code == 409 {
                        debug!(
                            channel = "telegram",
                            "Startup probe: 409 conflict (previous instance still running), retrying in 5s"
                        );
                    } else {
                        let desc = data
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        warn!(channel = "telegram", code = error_code, desc = %desc, "Startup probe API error, retrying in 5s");
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// 轮询新消息（timeout=30s 长轮询）
    async fn get_updates(&self, offset: i64) -> Result<Vec<TgUpdate>> {
        let url = self.api_url("getUpdates");
        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "offset": offset,
                "timeout": 30,
                "limit": 100,
                "allowed_updates": ["message", "callback_query"],
            }))
            .send()
            .await
            .map_err(|e| anyhow!("getUpdates request failed: {}", e))?;

        let tg: TgResponse<Vec<TgUpdate>> = resp
            .json()
            .await
            .map_err(|e| anyhow!("getUpdates JSON parse failed: {}", e))?;

        if !tg.ok {
            let code = tg.error_code.unwrap_or(0);
            let desc = tg.description.unwrap_or_default();
            // 409 特殊处理：有另一个实例在 polling
            if code == 409 {
                return Err(anyhow!("CONFLICT_409: {}", desc));
            }
            return Err(anyhow!("getUpdates error [{}]: {}", code, desc));
        }

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

    /// 编辑已存在的消息（失败时降级为新消息）
    async fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
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
            warn!(
                "editMessageText failed ({}), falling back to sendMessage",
                e
            );
            if let Err(e2) = self.send_message(chat_id, text, false).await {
                error!("fallback sendMessage also failed: {}", e2);
            }
        }
        Ok(())
    }

    // ── Typing 循环 ───────────────────────────────────────────────────────────

    /// 启动持续 typing 循环（每 4 秒刷新，直到被停止）
    ///
    /// Telegram 的 typing 状态 5 秒后过期，必须持续刷新才能保持"输入中"显示。
    async fn start_typing_loop(&self, chat_id: i64) {
        let chat_id_str = chat_id.to_string();
        // 先停止已有的 typing loop
        self.stop_typing_loop(&chat_id_str).await;

        let client = self.client.clone();
        let url = format!("https://api.telegram.org/bot{}/sendChatAction", self.token);

        let handle = tokio::spawn(async move {
            loop {
                let body = json!({ "chat_id": chat_id, "action": "typing" });
                let _ = client.post(&url).json(&body).send().await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });

        let mut tasks = self.typing_tasks.lock().await;
        tasks.insert(chat_id_str, handle);
    }

    /// 停止指定 chat 的 typing 循环
    async fn stop_typing_loop(&self, chat_id_str: &str) {
        let mut tasks = self.typing_tasks.lock().await;
        if let Some(handle) = tasks.remove(chat_id_str) {
            handle.abort();
        }
    }

    // ── Bot 命令处理 ──────────────────────────────────────────────────────────

    /// 处理 /start 命令
    async fn handle_command_start(&self, chat_id: i64) {
        let text = "👋 Hello! I'm AdaClaw, your AI agent.\n\nSend me a message and I'll get to work!\nType /help to see available commands.";
        let _ = self.send_message(chat_id, text, false).await;
    }

    /// 处理 /help 命令
    async fn handle_command_help(&self, chat_id: i64) {
        let text = "🤖 <b>AdaClaw Commands</b>\n\n/start — Start the bot\n/help — Show this help\n\nJust send a message to chat with the AI agent.";
        if let Err(e) = self.send_message(chat_id, text, true).await {
            error!(channel = "telegram", error = %e, "Failed to send help message");
        }
    }

    // ── mention-only 辅助 ─────────────────────────────────────────────────────

    /// 检查文本中是否包含对机器人的 @提及
    fn contains_bot_mention(text: &str, bot_username: &str) -> bool {
        let mention = format!("@{}", bot_username);
        text.to_lowercase().contains(&mention.to_lowercase())
    }

    /// 从文本中移除 @botname 提及，并折叠提及周围多余的空格。
    ///
    /// 例如："hi @MyBot status" → "hi status"（不留双空格）
    fn strip_bot_mention(text: &str, bot_username: &str) -> String {
        let mention = format!("@{}", bot_username);
        let re = Regex::new(&format!("(?i){}\\b", regex::escape(&mention)))
            .unwrap_or_else(|_| Regex::new("NOMATCH_IMPOSSIBLE").unwrap());
        let stripped = re.replace_all(text, "");
        // Collapse sequences of spaces introduced by removing the mention.
        // We only collapse horizontal spaces (not newlines) to preserve formatting.
        let space_re =
            Regex::new(r" {2,}").unwrap_or_else(|_| Regex::new("NOMATCH_IMPOSSIBLE").unwrap());
        space_re
            .replace_all(stripped.as_ref(), " ")
            .trim()
            .to_string()
    }

    // ── Approval Inline Keyboard ──────────────────────────────────────────────

    /// Send an approval prompt with ✅ Approve / ❌ Deny inline keyboard buttons.
    ///
    /// The callback data is `"acadapr:yes:{request_id}"` and `"acadapr:no:{request_id}"`.
    /// When the user presses a button, `process_callback_query()` converts it to a
    /// `/approve-allow {request_id}` or `/approve-deny {request_id}` inbound message
    /// that flows through the bus and is handled by the agent/dispatch layer.
    pub async fn send_approval_prompt_msg(
        &self,
        chat_id: i64,
        request_id: &str,
        tool_name: &str,
        args_preview: &str,
    ) -> Result<()> {
        let safe_tool = html_escape(tool_name);
        let safe_args = html_escape(&args_preview.chars().take(200).collect::<String>());
        let safe_id = html_escape(request_id);

        let text = format!(
            "🔒 <b>Tool approval required</b>\n\n\
             Tool: <code>{safe_tool}</code>\n\
             Args: <code>{safe_args}</code>\n\
             Request ID: <code>{safe_id}</code>\n\n\
             ⏱ This request expires in 30 minutes."
        );

        let params = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "reply_markup": {
                "inline_keyboard": [[
                    {
                        "text": "✅ Approve",
                        "callback_data": format!("{}{}", APPROVAL_CALLBACK_APPROVE_PREFIX, request_id)
                    },
                    {
                        "text": "❌ Deny",
                        "callback_data": format!("{}{}", APPROVAL_CALLBACK_DENY_PREFIX, request_id)
                    }
                ]]
            }
        });

        self.call_api::<TgSentMessage>("sendMessage", params)
            .await?;
        Ok(())
    }

    /// Handle a Telegram callback_query (inline keyboard button press).
    ///
    /// - Acknowledges the button press with `answerCallbackQuery` (shows a toast).
    /// - Clears the inline keyboard from the original message.
    /// - If the data matches an approval prefix, injects an `/approve-allow` or
    ///   `/approve-deny` inbound message into the bus for the agent to handle.
    async fn process_callback_query(&self, callback: &TgCallbackQuery, bus: &Arc<dyn MessageBus>) {
        let data = match &callback.data {
            Some(d) => d.as_str(),
            None => {
                self.answer_callback_query_nonblocking(callback.id.clone(), "");
                return;
            }
        };

        // Parse approval callback data
        let (command, request_id) = if let Some(rid) =
            data.strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
        {
            let rid = rid.trim();
            if rid.is_empty() {
                self.answer_callback_query_nonblocking(callback.id.clone(), "⚠️ Invalid request");
                return;
            }
            (format!("/approve-allow {}", rid), rid.to_string())
        } else if let Some(rid) = data.strip_prefix(APPROVAL_CALLBACK_DENY_PREFIX) {
            let rid = rid.trim();
            if rid.is_empty() {
                self.answer_callback_query_nonblocking(callback.id.clone(), "⚠️ Invalid request");
                return;
            }
            (format!("/approve-deny {}", rid), rid.to_string())
        } else {
            // Unknown callback — just acknowledge
            self.answer_callback_query_nonblocking(callback.id.clone(), "");
            return;
        };

        // Acknowledge the button press (shows a small toast on Telegram)
        let toast = if command.starts_with("/approve-allow") {
            "✅ Approval granted"
        } else {
            "❌ Request denied"
        };
        self.answer_callback_query_nonblocking(callback.id.clone(), toast);

        // Clear the inline keyboard from the original message so the buttons disappear
        if let Some(cb_msg) = &callback.message {
            self.clear_inline_keyboard_nonblocking(cb_msg.chat.id, cb_msg.message_id);
        }

        // Extract sender info
        let user = &callback.from;
        let sender_id = if let Some(uname) = &user.username {
            format!("{}|{}", user.id, uname)
        } else {
            user.id.to_string()
        };
        let sender_name = user.first_name.clone();

        // Determine chat_id for the reply target
        let chat_id_str = callback
            .message
            .as_ref()
            .map(|m| m.chat.id.to_string())
            .unwrap_or_else(|| user.id.to_string());

        // Check sender is in allowlist
        let allowed = self.base.is_allowed(&sender_id);
        if !allowed {
            warn!(
                sender_id = %sender_id,
                request_id = %request_id,
                "Approval callback from unauthorized sender ignored"
            );
            return;
        }

        debug!(
            sender_id = %sender_id,
            request_id = %request_id,
            command = %command,
            "Approval callback processed"
        );

        // Build metadata
        let mut metadata = HashMap::new();
        metadata.insert(
            "approval_request_id".to_string(),
            Value::String(request_id.clone()),
        );
        metadata.insert("is_approval_callback".to_string(), Value::Bool(true));

        // Inject the approval command into the bus as a regular inbound message
        self.base
            .handle_message(
                bus,
                &sender_id,
                &sender_name,
                &chat_id_str,
                &command,
                metadata,
            )
            .await;
    }

    /// Send a non-blocking `answerCallbackQuery` to acknowledge a button press.
    /// This shows a small toast notification on the user's Telegram client.
    fn answer_callback_query_nonblocking(&self, callback_query_id: String, text: &str) {
        let client = self.client.clone();
        let url = self.api_url("answerCallbackQuery");
        let text = text.to_string();
        tokio::spawn(async move {
            let body = json!({
                "callback_query_id": callback_query_id,
                "text": text,
                "show_alert": false,
            });
            if let Err(e) = client.post(&url).json(&body).send().await {
                debug!("answerCallbackQuery failed (non-blocking): {}", e);
            }
        });
    }

    /// Remove the inline keyboard from a message (non-blocking).
    /// Called after the user presses Approve/Deny so the buttons disappear.
    fn clear_inline_keyboard_nonblocking(&self, chat_id: i64, message_id: i64) {
        let client = self.client.clone();
        let url = self.api_url("editMessageReplyMarkup");
        tokio::spawn(async move {
            let body = json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reply_markup": {
                    "inline_keyboard": []
                }
            });
            if let Err(e) = client.post(&url).json(&body).send().await {
                debug!("clearInlineKeyboard failed (non-blocking): {}", e);
            }
        });
    }

    // ── 消息处理 ──────────────────────────────────────────────────────────────

    async fn process_update(&self, update: &TgUpdate, bus: &Arc<dyn MessageBus>) {
        // ── Route callback_query (inline keyboard button presses) ─────────────
        if let Some(callback) = &update.callback_query {
            self.process_callback_query(callback, bus).await;
            return;
        }

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

        // ── Bot 命令处理（/start、/help）─────────────────────────────────────
        if let Some(text) = &msg.text {
            let cmd = text.split_whitespace().next().unwrap_or("").to_lowercase();
            // 去掉 @botname 后缀（如 /start@MyBot）
            let base_cmd = cmd.split('@').next().unwrap_or(&cmd);
            match base_cmd {
                "/start" => {
                    self.handle_command_start(chat_id).await;
                    return;
                }
                "/help" => {
                    self.handle_command_help(chat_id).await;
                    return;
                }
                _ => {}
            }
        }

        // ── 白名单检查 ────────────────────────────────────────────────────────
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

        // ── 群组 mention-only 过滤 ────────────────────────────────────────────
        if is_group && self.mention_only {
            let text_to_check = msg.text.as_deref().or(msg.caption.as_deref()).unwrap_or("");

            let bot_uname = self.bot_username.lock().await.clone();
            match bot_uname {
                Some(ref uname) => {
                    if !Self::contains_bot_mention(text_to_check, uname) {
                        debug!(
                            channel = "telegram",
                            chat_id = %chat_id,
                            "Group message without bot mention, skipping (mention_only=true)"
                        );
                        return;
                    }
                }
                None => {
                    // 未获取到用户名，跳过（保守策略）
                    debug!(
                        channel = "telegram",
                        "Bot username not available, skipping group message"
                    );
                    return;
                }
            }
        }

        // ── 提取文本内容 ──────────────────────────────────────────────────────
        let mut content_parts: Vec<String> = Vec::new();

        // 提取文字（群组中去除 @提及）
        let raw_text = msg.text.as_deref().or(msg.caption.as_deref()).unwrap_or("");
        let text_content = if is_group && self.mention_only {
            let bot_uname = self.bot_username.lock().await.clone().unwrap_or_default();
            Self::strip_bot_mention(raw_text, &bot_uname)
        } else {
            raw_text.to_string()
        };
        if !text_content.is_empty() {
            content_parts.push(text_content);
        }

        // ── 处理媒体附件 ──────────────────────────────────────────────────────
        let mut metadata_extra: HashMap<String, Value> = HashMap::new();
        if let Some(photos) = &msg.photo
            && let Some(largest) = photos.last()
        {
            content_parts.push("[image]".to_string());
            metadata_extra.insert(
                "photo_file_id".to_string(),
                Value::String(largest.file_id.clone()),
            );
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
            preview = %content.chars().take(60).collect::<String>(),
            "Telegram message received"
        );

        // ── 启动持续 Typing 循环 ──────────────────────────────────────────────
        self.start_typing_loop(chat_id).await;

        // ── 发送 "Thinking..." 占位消息 ───────────────────────────────────────
        match self.send_message(chat_id, "Thinking... 💭", false).await {
            Ok(mid) => {
                let mut pmap = self.placeholders.lock().unwrap();
                pmap.insert(chat_id_str.clone(), mid);
            }
            Err(e) => {
                warn!(error = %e, "Failed to send Thinking placeholder");
            }
        }

        // ── 构建 metadata ─────────────────────────────────────────────────────
        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert(
            "message_id".to_string(),
            Value::String(msg.message_id.to_string()),
        );
        metadata.insert("user_id".to_string(), Value::String(user.id.to_string()));
        metadata.insert(
            "first_name".to_string(),
            Value::String(user.first_name.clone()),
        );
        metadata.insert("is_group".to_string(), Value::Bool(is_group));
        for (k, v) in metadata_extra {
            metadata.insert(k, v);
        }

        // ── 上报到 Bus ────────────────────────────────────────────────────────
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

    /// 将回复内容分片发送（每片最多 TG_MAX_MESSAGE_CHARS 字符）
    async fn send_reply(&self, chat_id: i64, chat_id_str: &str, content: &str) {
        // 先停止 typing 循环
        self.stop_typing_loop(chat_id_str).await;

        let chunks = split_message(content, TG_MAX_MESSAGE_CHARS);

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

        // mention-only 模式需要先获取 bot username
        if self.mention_only {
            if let Some(uname) = self.get_bot_username().await {
                info!(channel = "telegram", username = %uname, "mention_only mode enabled");
            } else {
                warn!(
                    channel = "telegram",
                    "mention_only=true but failed to fetch bot username; group messages will be skipped"
                );
            }
        }

        // 启动探针：等待 getUpdates slot 可用（避免 409 Conflict）
        info!(
            channel = "telegram",
            "Running startup probe (detecting 409 conflicts)..."
        );
        let mut offset = self.startup_probe().await;
        info!(
            channel = "telegram",
            offset = offset,
            "Startup probe done, entering long-poll loop"
        );

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
                    let err_str = e.to_string();
                    consecutive_errors += 1;

                    if err_str.starts_with("CONFLICT_409") {
                        // 409 冲突：等待 35s 让对方的 30s 长轮询超时
                        warn!(
                            channel = "telegram",
                            "409 Conflict: another instance is polling. Waiting 35s for it to expire..."
                        );
                        tokio::time::sleep(Duration::from_secs(35)).await;
                    } else {
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
        }

        self.base.set_running(false);
        info!("Telegram channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if !self.base.is_running() {
            return Err(anyhow!("Telegram channel is not running"));
        }

        let chat_id: i64 = msg
            .target_session_id
            .parse()
            .map_err(|_| anyhow!("Invalid Telegram chat_id: '{}'", msg.target_session_id))?;
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
        // 停止所有 typing 循环
        let mut tasks = self.typing_tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
        info!("Telegram channel stop requested");
        Ok(())
    }

    /// Send an approval prompt with ✅ Approve / ❌ Deny inline keyboard buttons.
    ///
    /// Overrides the default no-op from the `Channel` trait.
    /// Parses `session_id` as a Telegram `chat_id` (i64) and calls
    /// `send_approval_prompt_msg()`.
    async fn send_approval_prompt(
        &self,
        session_id: &str,
        request_id: &str,
        tool_name: &str,
        args_preview: &str,
    ) -> Result<()> {
        let chat_id: i64 = session_id.parse().map_err(|_| {
            anyhow!(
                "Invalid Telegram chat_id for approval prompt: '{}'",
                session_id
            )
        })?;
        self.send_approval_prompt_msg(chat_id, request_id, tool_name, args_preview)
            .await
    }

    fn supports_approval_prompts(&self) -> bool {
        true
    }
}

// ── HTML 辅助 ─────────────────────────────────────────────────────────────────

/// Escape special HTML characters for safe use in Telegram HTML parse_mode.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Markdown → Telegram HTML 转换 ─────────────────────────────────────────────

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
    result = re_heading.replace_all(&result, "$1").into_owned();

    // Step 4: 移除 > 引用标记
    let re_blockquote = Regex::new(r"(?m)^>\s*(.*)$").unwrap();
    result = re_blockquote.replace_all(&result, "$1").into_owned();

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
    result = re_bold_under.replace_all(&result, "<b>$1</b>").into_owned();

    // Step 8: 斜体 _text_（避免匹配 some_var_name）
    let re_italic = Regex::new(r"(^|[^a-zA-Z0-9])_([^_\n]+)_((?:[^a-zA-Z0-9])|$)").unwrap();
    result = re_italic
        .replace_all(&result, "${1}<i>${2}</i>${3}")
        .into_owned();

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

/// 将长文本按 Unicode 字符数分片（最多 max_chars 个字符），
/// 优先在换行符处断开，其次在空格处，最后强制截断。
fn split_message(content: &str, max_chars: usize) -> Vec<String> {
    // 按字符数而非字节数判断，对中文/emoji 等 Unicode 字符正确处理
    if content.chars().count() <= max_chars {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        // 找到第 max_chars 个字符的字节偏移
        let hard_byte_pos = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..hard_byte_pos];

        // 优先在换行符处断开
        let cut_pos = if let Some(nl) = candidate.rfind('\n') {
            // 只有换行符位置足够靠后才用（至少是候选区一半）才用
            if candidate[..nl].chars().count() >= max_chars / 2 {
                nl + 1 // 包含换行符
            } else {
                // 换行符太靠前，尝试空格
                candidate.rfind(' ').map(|p| p + 1).unwrap_or(hard_byte_pos)
            }
        } else if let Some(sp) = candidate.rfind(' ') {
            sp + 1
        } else {
            hard_byte_pos // 强制截断
        };

        chunks.push(remaining[..cut_pos].to_string());
        remaining = remaining[cut_pos..].trim_start_matches(['\n', ' ']);
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
        let chunks = split_message("hello", TG_MAX_MESSAGE_CHARS);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn test_split_message_ascii_long() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TG_MAX_MESSAGE_CHARS);
        }
    }

    #[test]
    fn test_split_message_unicode() {
        // 中文字符每个 3 字节，但应按字符数（Unicode code points）分片
        let text = "中".repeat(5000);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= TG_MAX_MESSAGE_CHARS,
                "chunk has {} chars, max is {}",
                chunk.chars().count(),
                TG_MAX_MESSAGE_CHARS
            );
        }
        // 验证内容完整
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_split_message_exactly_at_limit() {
        let text = "x".repeat(TG_MAX_MESSAGE_CHARS);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_split_message_one_over_limit() {
        let text = "x".repeat(TG_MAX_MESSAGE_CHARS + 1);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TG_MAX_MESSAGE_CHARS);
        }
    }

    #[test]
    fn test_split_preserves_content() {
        let text = "word ".repeat(TG_MAX_MESSAGE_CHARS / 5 + 100);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        let rejoined = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_allowlist_compound() {
        let ch =
            TelegramChannel::new("token".to_string()).with_allow_from(vec!["123456".to_string()]);
        assert!(ch.base.is_allowed("123456|alice"));
        assert!(!ch.base.is_allowed("999|alice"));
    }

    #[test]
    fn test_contains_bot_mention() {
        assert!(TelegramChannel::contains_bot_mention(
            "hi @MyBot please help",
            "mybot"
        ));
        assert!(TelegramChannel::contains_bot_mention(
            "@mybot do this",
            "mybot"
        ));
        assert!(!TelegramChannel::contains_bot_mention(
            "hello world",
            "mybot"
        ));
        assert!(!TelegramChannel::contains_bot_mention("@otherbot", "mybot"));
    }

    #[test]
    fn test_strip_bot_mention() {
        let result = TelegramChannel::strip_bot_mention("@mybot please help", "mybot");
        assert_eq!(result, "please help");
        let result2 = TelegramChannel::strip_bot_mention("hi @MyBot status", "mybot");
        assert_eq!(result2, "hi status");
    }

    #[test]
    fn test_split_message_limit_is_4096() {
        // 验证常量确实是 4096
        assert_eq!(TG_MAX_MESSAGE_CHARS, 4096);
        // 4096 字符的文本不应分片
        let text = "a".repeat(4096);
        let chunks = split_message(&text, TG_MAX_MESSAGE_CHARS);
        assert_eq!(chunks.len(), 1);
        // 4097 字符的文本应分片
        let text2 = "a".repeat(4097);
        let chunks2 = split_message(&text2, TG_MAX_MESSAGE_CHARS);
        assert!(chunks2.len() > 1);
    }

    // ── Approval callback parsing ─────────────────────────────────────────────

    #[test]
    fn test_approval_callback_approve_prefix_parses_request_id() {
        let data = format!("{}apr-1a2b3c", APPROVAL_CALLBACK_APPROVE_PREFIX);
        assert!(
            data.strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
                .is_some()
        );
        let rid = data.strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX).unwrap();
        assert_eq!(rid, "apr-1a2b3c");
    }

    #[test]
    fn test_approval_callback_deny_prefix_parses_request_id() {
        let data = format!("{}apr-dead00", APPROVAL_CALLBACK_DENY_PREFIX);
        let rid = data.strip_prefix(APPROVAL_CALLBACK_DENY_PREFIX).unwrap();
        assert_eq!(rid, "apr-dead00");
    }

    #[test]
    fn test_approval_callback_approve_prefix_constant() {
        assert_eq!(APPROVAL_CALLBACK_APPROVE_PREFIX, "acadapr:yes:");
    }

    #[test]
    fn test_approval_callback_deny_prefix_constant() {
        assert_eq!(APPROVAL_CALLBACK_DENY_PREFIX, "acadapr:no:");
    }

    #[test]
    fn test_approval_callback_data_distinguishes_approve_deny() {
        let approve_data = format!("{}apr-aaa", APPROVAL_CALLBACK_APPROVE_PREFIX);
        let deny_data = format!("{}apr-aaa", APPROVAL_CALLBACK_DENY_PREFIX);

        assert!(
            approve_data
                .strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
                .is_some()
        );
        assert!(
            approve_data
                .strip_prefix(APPROVAL_CALLBACK_DENY_PREFIX)
                .is_none()
        );

        assert!(
            deny_data
                .strip_prefix(APPROVAL_CALLBACK_DENY_PREFIX)
                .is_some()
        );
        assert!(
            deny_data
                .strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
                .is_none()
        );
    }

    #[test]
    fn test_approval_callback_empty_request_id_rejected() {
        // Empty request_id after prefix — should NOT be treated as valid
        let approve_data = APPROVAL_CALLBACK_APPROVE_PREFIX; // no request_id
        let rid = approve_data
            .strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
            .unwrap_or("")
            .trim();
        assert!(
            rid.is_empty(),
            "Empty request_id should be treated as invalid"
        );
    }

    #[test]
    fn test_approval_callback_whitespace_trimmed_request_id_rejected() {
        let data = format!("{}   ", APPROVAL_CALLBACK_APPROVE_PREFIX);
        let rid = data
            .strip_prefix(APPROVAL_CALLBACK_APPROVE_PREFIX)
            .unwrap_or("")
            .trim();
        assert!(
            rid.is_empty(),
            "Whitespace-only request_id should be treated as invalid"
        );
    }

    #[test]
    fn test_html_escape_basic() {
        assert_eq!(html_escape("<b>"), "&lt;b&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape("\"quoted\""), "&quot;quoted&quot;");
        assert_eq!(html_escape("no special chars"), "no special chars");
    }

    #[test]
    fn test_html_escape_xss_prevention() {
        let malicious = "<script>alert('xss')</script>";
        let escaped = html_escape(malicious);
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert!(escaped.contains("&lt;script&gt;"));
    }

    #[test]
    fn test_approval_prompt_callback_data_max_length() {
        // Telegram callback_data is limited to 64 bytes
        let request_id = "apr-12345"; // 9 chars
        let approve_data = format!("{}{}", APPROVAL_CALLBACK_APPROVE_PREFIX, request_id);
        assert!(
            approve_data.len() <= 64,
            "callback_data must be <= 64 bytes, got {}",
            approve_data.len()
        );
        let deny_data = format!("{}{}", APPROVAL_CALLBACK_DENY_PREFIX, request_id);
        assert!(
            deny_data.len() <= 64,
            "callback_data must be <= 64 bytes, got {}",
            deny_data.len()
        );
    }
}
