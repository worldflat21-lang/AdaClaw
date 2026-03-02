//! Discord Bot 渠道 — Gateway WebSocket 模式
//!
//! # 工作原理
//!
//! 1. 连接 Discord Gateway WebSocket（wss://gateway.discord.gg/?v=10&encoding=json）
//! 2. 收到 HELLO (op=10) → 启动心跳，发送 IDENTIFY (op=2)
//! 3. 监听 MESSAGE_CREATE 事件，过滤 Bot 消息，发布到 MessageBus
//! 4. `send()` 方法通过 REST API 回复消息
//! 5. 断线自动重连（指数退避）
//! 6. **持续 Typing 循环**：每 8 秒刷新 typing，Discord 的 typing 也是 5s 过期
//!
//! # 配置示例
//!
//! ```toml
//! [channels.discord]
//! kind = "discord"
//! token = "Bot xxxxxx"           # Discord Bot Token（含 "Bot " 前缀）
//! allow_from = []
//!
//! [channels.discord.extra]
//! intents = "512"                # MESSAGE_CONTENT intent (512 = 1<<9)
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const DISCORD_API: &str = "https://discord.com/api/v10";
const DISCORD_GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Discord Gateway Intents
/// MESSAGE_CONTENT (1<<15) + GUILD_MESSAGES (1<<9) + DIRECT_MESSAGES (1<<12)
const DEFAULT_INTENTS: u64 = (1 << 15) | (1 << 9) | (1 << 12);

pub struct DiscordChannel {
    base: BaseChannel,
    token: String,
    intents: u64,
    client: reqwest::Client,
    /// channel_id → typing loop task handle（持续刷新 typing，直到回复发出）
    typing_tasks: Arc<tokio::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl DiscordChannel {
    pub fn new(token: String, allow_from: Vec<String>, intents: Option<u64>) -> Self {
        // reqwest::Client::builder().build() only fails when custom TLS/proxy
        // configuration is invalid.  With a simple timeout, it is effectively
        // infallible; fall back to the default client if it somehow does fail.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base: BaseChannel::new("discord").with_allow_from(allow_from),
            token,
            intents: intents.unwrap_or(DEFAULT_INTENTS),
            client,
            typing_tasks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    fn auth_header(&self) -> String {
        if self.token.starts_with("Bot ") {
            self.token.clone()
        } else {
            format!("Bot {}", self.token)
        }
    }

    // ── Typing 循环 ───────────────────────────────────────────────────────────

    /// 启动持续 typing 循环（每 8 秒刷新）
    ///
    /// Discord 的 typing 指示器 5 秒后过期，须持续刷新。
    /// 收到用户消息时启动，发出回复时停止。
    async fn start_typing_loop(&self, channel_id: &str) {
        self.stop_typing_loop(channel_id).await;

        let client = self.client.clone();
        let url = format!("{}/channels/{}/typing", DISCORD_API, channel_id);
        let auth = self.auth_header();

        let handle = tokio::spawn(async move {
            loop {
                // Discord typing POST 不需要 body
                let _ = client
                    .post(&url)
                    .header("Authorization", &auth)
                    .header("Content-Length", "0")
                    .send()
                    .await;
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
        });

        let mut tasks = self.typing_tasks.lock().await;
        tasks.insert(channel_id.to_string(), handle);
    }

    /// 停止指定 channel 的 typing 循环
    async fn stop_typing_loop(&self, channel_id: &str) {
        let mut tasks = self.typing_tasks.lock().await;
        if let Some(handle) = tasks.remove(channel_id) {
            handle.abort();
        }
    }

    /// 通过 REST API 发送消息到 Discord 频道
    async fn send_rest(&self, channel_id: &str, content: &str) -> Result<()> {
        // Discord 消息最长 2000 字符，超过时分片
        let chunks = split_discord_message(content, 2000);
        let url = format!("{}/channels/{}/messages", DISCORD_API, channel_id);

        for chunk in chunks {
            let resp = self
                .client
                .post(&url)
                .header("Authorization", self.auth_header())
                .json(&json!({ "content": chunk }))
                .send()
                .await
                .map_err(|e| anyhow!("Discord send request failed: {}", e))?;

            if resp.status() == 429 {
                // Rate limited
                let body: Value = resp.json().await.unwrap_or_default();
                let retry_after = body
                    .get("retry_after")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0);
                warn!(channel = "discord", retry_after = retry_after, "Rate limited");
                tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                // Retry once
                let _ = self
                    .client
                    .post(&url)
                    .header("Authorization", self.auth_header())
                    .json(&json!({ "content": chunk }))
                    .send()
                    .await;
            } else if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "Discord send failed: HTTP {} — {}",
                    status,
                    body
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        if self.token.is_empty() {
            return Err(anyhow!("Discord bot token is not configured"));
        }

        self.base.set_running(true);
        info!("Starting Discord channel (Gateway WebSocket)...");

        let mut retry_delay = 1u64;

        while self.base.is_running() {
            match self.run_gateway_session(&bus).await {
                Ok(()) => {
                    if !self.base.is_running() {
                        break;
                    }
                    info!(channel = "discord", "Gateway session ended, reconnecting...");
                    retry_delay = 1; // 正常断线，重置退避
                }
                Err(e) => {
                    error!(channel = "discord", error = %e, "Gateway session error");
                    retry_delay = std::cmp::min(retry_delay * 2, 60);
                }
            }

            if self.base.is_running() {
                warn!(
                    channel = "discord",
                    retry_in = retry_delay,
                    "Reconnecting to Discord Gateway in {}s", retry_delay
                );
                tokio::time::sleep(Duration::from_secs(retry_delay)).await;
            }
        }

        self.base.set_running(false);
        info!("Discord channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let channel_id = &msg.target_session_id;
        // 停止该 channel 的 typing 循环
        self.stop_typing_loop(channel_id).await;

        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };
        self.send_rest(channel_id, &content).await
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        // 停止所有 typing 循环
        let mut tasks = self.typing_tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
        Ok(())
    }
}

impl DiscordChannel {
    async fn run_gateway_session(&self, bus: &Arc<dyn MessageBus>) -> Result<()> {
        let (ws_stream, _) = connect_async(DISCORD_GATEWAY)
            .await
            .map_err(|e| anyhow!("Discord Gateway connection failed: {}", e))?;

        let (mut writer, mut reader) = ws_stream.split();

        // 用 mpsc 发送 WebSocket 消息，让心跳任务和主循环共享写端
        let (tx, mut rx) = mpsc::channel::<String>(32);

        // 消息写入任务
        let write_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if writer.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
        });

        let intents = self.intents;
        let token = self.token.clone();
        let base = &self.base;

        let mut heartbeat_handle: Option<tokio::task::JoinHandle<()>> = None;

        while base.is_running() {
            let msg = tokio::time::timeout(Duration::from_secs(120), reader.next())
                .await
                .map_err(|_| anyhow!("Discord Gateway read timeout"))?;

            let raw_msg = match msg {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(anyhow!("Discord WebSocket error: {}", e)),
                None => return Ok(()), // 连接关闭
            };

            let text = match raw_msg {
                Message::Text(t) => t,
                Message::Close(_) => return Ok(()),
                Message::Ping(_) => {
                    // 自动 Pong
                    let _ = tx.send(
                        serde_json::to_string(&json!({ "op": 1, "d": null })).unwrap_or_default()
                    ).await;
                    continue;
                }
                _ => continue,
            };

            let payload: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(channel = "discord", error = %e, "Failed to parse Gateway payload");
                    continue;
                }
            };

            let op = payload.get("op").and_then(|v| v.as_u64()).unwrap_or(999);
            let event_type = payload
                .get("t")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let data = payload.get("d");

            // 更新序列号（当前实现不使用 seq，将来 RESUME 时需要）
            let _ = payload.get("s").and_then(|v| v.as_i64());

            match op {
                // HELLO
                10 => {
                    let interval = data
                        .and_then(|d| d.get("heartbeat_interval"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(45000);

                    // 发送 IDENTIFY
                    let identify = json!({
                        "op": 2,
                        "d": {
                            "token": token,
                            "intents": intents,
                            "properties": {
                                "os": "linux",
                                "browser": "adaclaw",
                                "device": "adaclaw"
                            }
                        }
                    });
                    let _ = tx
                        .send(serde_json::to_string(&identify).unwrap_or_default())
                        .await;

                    // 启动心跳任务
                    if let Some(h) = heartbeat_handle.take() {
                        h.abort();
                    }
                    let hb_tx = tx.clone();
                    heartbeat_handle = Some(tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(interval)).await;
                            let hb = json!({ "op": 1, "d": null });
                            if hb_tx
                                .send(serde_json::to_string(&hb).unwrap_or_default())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }));

                    debug!(channel = "discord", "HELLO received, IDENTIFY sent");
                }

                // DISPATCH
                0 => {
                    match event_type.as_str() {
                        "READY" => {
                            info!(channel = "discord", "Discord Gateway READY");
                        }
                        "MESSAGE_CREATE" => {
                            if let Some(d) = data {
                                self.handle_message_create(d, bus).await;
                            }
                        }
                        _ => {}
                    }
                }

                // HEARTBEAT ACK
                11 => {
                    debug!(channel = "discord", "Heartbeat ACK");
                }

                // RECONNECT
                7 => {
                    info!(channel = "discord", "Server requested reconnect");
                    break;
                }

                // INVALID SESSION
                9 => {
                    warn!(channel = "discord", "Invalid session, reconnecting");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    break;
                }

                _ => {}
            }
        }

        if let Some(h) = heartbeat_handle {
            h.abort();
        }
        drop(tx);
        let _ = write_task.await;
        Ok(())
    }

    async fn handle_message_create(&self, data: &Value, bus: &Arc<dyn MessageBus>) {
        // 忽略 Bot 自身消息
        if data
            .pointer("/author/bot")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return;
        }

        let sender_id = data
            .pointer("/author/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sender_name = data
            .pointer("/author/username")
            .and_then(|v| v.as_str())
            .unwrap_or(&sender_id)
            .to_string();
        let channel_id = data
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let message_id = data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if content.is_empty() || sender_id.is_empty() || channel_id.is_empty() {
            return;
        }

        debug!(
            channel = "discord",
            sender_id = %sender_id,
            channel_id = %channel_id,
            preview = %content.chars().take(60).collect::<String>(),
            "Discord message received"
        );

        // 启动持续 typing 循环（在白名单检查之前就启动，给用户及时反馈）
        self.start_typing_loop(&channel_id).await;

        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert("message_id".to_string(), Value::String(message_id));
        metadata.insert(
            "guild_id".to_string(),
            Value::String(
                data.get("guild_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
        );

        // session_id = channel_id（send() 方法用来调用 REST API）
        self.base
            .handle_message(bus, &sender_id, &sender_name, &channel_id, &content, metadata)
            .await;
    }
}

/// Split a Discord message into chunks of at most `max_chars` **Unicode scalar
/// values** (characters), which is what Discord's 2000-character limit refers to.
///
/// Splitting is done at character boundaries (never mid-codepoint), preferring
/// newline → space → hard cut, in that order.
fn split_discord_message(content: &str, max_chars: usize) -> Vec<String> {
    // Use .chars().count() — byte length is wrong for CJK / emoji characters.
    if content.chars().count() <= max_chars {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        // Find the byte offset of the max_chars-th character boundary.
        // char_indices() is safe — it always returns valid UTF-8 boundaries.
        let hard_byte_pos = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..hard_byte_pos];

        // Prefer splitting at a newline, then at a space, then hard-cut.
        let cut_byte_pos = if let Some(nl) = candidate.rfind('\n') {
            // Only use the newline if it's at least halfway through the chunk.
            if candidate[..nl].chars().count() >= max_chars / 2 {
                nl + 1 // include the newline in the chunk
            } else {
                candidate.rfind(' ').map(|p| p + 1).unwrap_or(hard_byte_pos)
            }
        } else if let Some(sp) = candidate.rfind(' ') {
            sp + 1
        } else {
            hard_byte_pos // hard cut at character boundary
        };

        chunks.push(remaining[..cut_byte_pos].to_string());
        remaining = remaining[cut_byte_pos..].trim_start_matches(['\n', ' ']);
    }

    chunks
}
