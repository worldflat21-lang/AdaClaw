//! Discord Bot 渠道 — Gateway WebSocket 模式
//!
//! # 工作原理
//!
//! 1. 连接 Discord Gateway WebSocket（wss://gateway.discord.gg/?v=10&encoding=json）
//! 2. 收到 HELLO (op=10) → 启动心跳，发送 IDENTIFY (op=2)
//! 3. 监听 MESSAGE_CREATE 事件，过滤 Bot 消息，发布到 MessageBus
//! 4. `send()` 方法通过 REST API 回复消息
//! 5. 断线自动重连（指数退避）
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
}

impl DiscordChannel {
    pub fn new(token: String, allow_from: Vec<String>, intents: Option<u64>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base: BaseChannel::new("discord").with_allow_from(allow_from),
            token,
            intents: intents.unwrap_or(DEFAULT_INTENTS),
            client,
        }
    }

    fn auth_header(&self) -> String {
        if self.token.starts_with("Bot ") {
            self.token.clone()
        } else {
            format!("Bot {}", self.token)
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
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };
        self.send_rest(channel_id, &content).await
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
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
        let client = &self.client;

        let mut heartbeat_interval_ms: Option<u64> = None;
        let mut heartbeat_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut seq: Option<i64> = None;

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
                Message::Ping(d) => {
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

            // 更新序列号
            if let Some(s) = payload.get("s").and_then(|v| v.as_i64()) {
                seq = Some(s);
            }

            match op {
                // HELLO
                10 => {
                    let interval = data
                        .and_then(|d| d.get("heartbeat_interval"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(45000);
                    heartbeat_interval_ms = Some(interval);

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

fn split_discord_message(content: &str, max_len: usize) -> Vec<String> {
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
        let pos = cut.rfind('\n').unwrap_or_else(|| cut.rfind(' ').unwrap_or(max_len));
        chunks.push(remaining[..pos].to_string());
        remaining = remaining[pos..].trim_start();
    }
    chunks
}
