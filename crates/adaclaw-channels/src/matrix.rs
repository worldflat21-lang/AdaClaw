//! Matrix 渠道 — Client-Server API（长轮询）
//!
//! # 功能特性（feature = "matrix"）
//!
//! - Matrix Client-Server API r0/v3 长轮询（`/_matrix/client/v3/sync`）
//! - 支持文本消息（`m.room.message` / `m.text`）
//! - 发送消息通过 PUT `rooms/{roomId}/send/m.room.message/{txnId}`
//! - 访问令牌（`access_token`）持久跨重启
//! - 支持多 room 过滤（`allow_from` 匹配 room ID 或用户 ID）
//! - E2EE（端对端加密）：未在本版本实现，可通过 `vodozemac` crate 扩展
//!
//! # 配置示例
//!
//! ```toml
//! [channels.matrix]
//! kind = "matrix"
//! token = "syt_..."               # Matrix access_token
//! allow_from = ["@admin:matrix.org", "!roomid:matrix.org"]
//!
//! [channels.matrix.extra]
//! homeserver = "https://matrix.org"   # Matrix homeserver URL
//! user_id = "@mybot:matrix.org"       # Bot 用户 ID
//! device_id = "ADACLAWDEV01"          # 设备 ID（稳定跨重启）
//! sync_timeout_ms = "30000"           # 长轮询超时（毫秒）
//! ```
//!
//! # 获取 access_token
//!
//! ```bash
//! curl -XPOST 'https://matrix.org/_matrix/client/v3/login' \
//!   -H 'Content-Type: application/json' \
//!   -d '{"type":"m.login.password","user":"@you:matrix.org","password":"your-password"}'
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

// ── Matrix API 响应结构 ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SyncResponse {
    next_batch: String,
    rooms: Option<SyncRooms>,
}

#[derive(Debug, Deserialize)]
struct SyncRooms {
    join: Option<HashMap<String, JoinedRoom>>,
}

#[derive(Debug, Deserialize)]
struct JoinedRoom {
    timeline: Option<Timeline>,
}

#[derive(Debug, Deserialize)]
struct Timeline {
    events: Option<Vec<RoomEvent>>,
}

#[derive(Debug, Deserialize)]
struct RoomEvent {
    event_id: Option<String>,
    sender: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    content: Option<Value>,
    origin_server_ts: Option<u64>,
}

// ── MatrixChannel ─────────────────────────────────────────────────────────────

/// Matrix Client-Server API 渠道
///
/// 配置字段（从 `ChannelConfig` 读取）：
/// - `token` → `access_token`
/// - `extra.homeserver` → Matrix homeserver URL（如 `https://matrix.org`）
/// - `extra.user_id` → Bot 用户 ID（如 `@bot:matrix.org`）
/// - `extra.device_id` → 设备 ID（可选，稳定跨重启）
/// - `extra.sync_timeout_ms` → 长轮询超时毫秒数（默认 30000）
/// - `allow_from` → 白名单（支持 room ID 或用户 ID）
pub struct MatrixChannel {
    base: Arc<BaseChannel>,
    homeserver: String,
    access_token: String,
    user_id: String,
    device_id: String,
    sync_timeout_ms: u64,
    http_client: reqwest::Client,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl MatrixChannel {
    /// 从 `ChannelConfig.extra` 构建 MatrixChannel。
    pub fn from_extra(
        token: Option<String>,
        allow_from: Vec<String>,
        extra: &HashMap<String, String>,
    ) -> Result<Self> {
        let access_token = token
            .ok_or_else(|| anyhow!("channels.matrix.token (access_token) is required"))?;
        let homeserver = extra
            .get("homeserver")
            .cloned()
            .ok_or_else(|| anyhow!("channels.matrix.extra.homeserver is required"))?;
        let homeserver = homeserver.trim_end_matches('/').to_string();
        let user_id = extra
            .get("user_id")
            .cloned()
            .unwrap_or_default();
        let device_id = extra
            .get("device_id")
            .cloned()
            .unwrap_or_else(|| "ADACLAW".to_string());
        let sync_timeout_ms = extra
            .get("sync_timeout_ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000u64);

        let base = Arc::new(BaseChannel::new("matrix").with_allow_from(allow_from));
        let client = reqwest::Client::builder()
            // 长轮询超时要大于 sync_timeout_ms
            .timeout(Duration::from_millis(sync_timeout_ms + 10_000))
            .build()
            .expect("Failed to build HTTP client");

        Ok(Self {
            base,
            homeserver,
            access_token,
            user_id,
            device_id,
            sync_timeout_ms,
            http_client: client,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// 发送 Matrix 消息（PUT 到房间）
    async fn send_message(&self, room_id: &str, text: &str) -> Result<()> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver,
            urlencoding::encode(room_id),
            txn_id
        );

        let resp = self
            .http_client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&json!({
                "msgtype": "m.text",
                "body": text
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Matrix send_message request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            return Err(anyhow!("Matrix API error {}: {}", status, body));
        }
        Ok(())
    }

    /// 执行一次 /sync 请求（长轮询）
    async fn sync_once(
        &self,
        since: Option<&str>,
    ) -> Result<SyncResponse> {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={}",
            self.homeserver, self.sync_timeout_ms
        );
        if let Some(batch) = since {
            url.push_str(&format!("&since={}", urlencoding::encode(batch)));
        }

        let resp = self
            .http_client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| anyhow!("Matrix sync request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            return Err(anyhow!("Matrix sync error {}: {}", status, body));
        }

        resp.json::<SyncResponse>()
            .await
            .map_err(|e| anyhow!("Matrix sync response parse error: {}", e))
    }
}

#[async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        self.base.set_running(true);
        info!(
            channel = "matrix",
            homeserver = %self.homeserver,
            user_id = %self.user_id,
            "Matrix channel started"
        );

        let (tx, mut rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        let mut next_batch: Option<String> = None;
        let bot_user_id = self.user_id.clone();

        loop {
            // 检查停止信号
            if rx.try_recv().is_ok() {
                info!(channel = "matrix", "Matrix channel stop signal received");
                break;
            }

            // 发起 sync 请求
            let sync_result = tokio::select! {
                result = self.sync_once(next_batch.as_deref()) => result,
                _ = &mut rx => {
                    info!(channel = "matrix", "Matrix channel stopped during sync");
                    break;
                }
            };

            let sync_resp = match sync_result {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        channel = "matrix",
                        error = %e,
                        "Matrix sync error, retrying in 10s"
                    );
                    // 指数退避
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            // 更新 next_batch（用于下次 sync）
            let new_batch = sync_resp.next_batch.clone();

            // 处理 joined rooms 的消息事件
            if let Some(rooms) = sync_resp.rooms {
                if let Some(join_map) = rooms.join {
                    for (room_id, joined_room) in join_map {
                        let events = joined_room
                            .timeline
                            .and_then(|t| t.events)
                            .unwrap_or_default();

                        for event in events {
                            // 只处理 m.room.message
                            if event.event_type.as_deref() != Some("m.room.message") {
                                continue;
                            }

                            let sender = event.sender.unwrap_or_default();

                            // 忽略自己发的消息
                            if !bot_user_id.is_empty() && sender == bot_user_id {
                                continue;
                            }

                            let content = match &event.content {
                                Some(c) => c,
                                None => continue,
                            };

                            // 只处理 m.text 类型
                            if content.get("msgtype").and_then(|v| v.as_str()) != Some("m.text") {
                                continue;
                            }

                            let text = match content.get("body").and_then(|v| v.as_str()) {
                                Some(t) if !t.trim().is_empty() => t.to_string(),
                                _ => continue,
                            };

                            debug!(
                                channel = "matrix",
                                room_id = %room_id,
                                sender = %sender,
                                preview = %text.chars().take(60).collect::<String>(),
                                "Matrix message received"
                            );

                            // 白名单检查（支持 room_id 或 sender 匹配）
                            let allowed = self.base.is_allowed(&sender)
                                || self.base.is_allowed(&room_id);
                            if !allowed {
                                warn!(
                                    channel = "matrix",
                                    sender = %sender,
                                    room_id = %room_id,
                                    "Not in allowlist, ignoring"
                                );
                                continue;
                            }

                            let mut metadata: HashMap<String, Value> = HashMap::new();
                            if let Some(event_id) = &event.event_id {
                                metadata.insert(
                                    "event_id".to_string(),
                                    Value::String(event_id.clone()),
                                );
                            }
                            metadata.insert(
                                "room_id".to_string(),
                                Value::String(room_id.clone()),
                            );
                            if let Some(ts) = event.origin_server_ts {
                                metadata.insert(
                                    "origin_server_ts".to_string(),
                                    Value::Number(ts.into()),
                                );
                            }

                            // session_id = room_id，send() 用于回复到同一房间
                            self.base
                                .handle_message(
                                    &bus,
                                    &sender,
                                    &sender,
                                    &room_id,
                                    text.trim(),
                                    metadata,
                                )
                                .await;
                        }
                    }
                }
            }

            next_batch = Some(new_batch);
        }

        self.base.set_running(false);
        info!(channel = "matrix", "Matrix channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let room_id = &msg.target_session_id;
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        self.send_message(room_id, &content).await
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.base.set_running(false);
        Ok(())
    }
}
