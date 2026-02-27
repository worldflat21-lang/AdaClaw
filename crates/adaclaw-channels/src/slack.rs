//! Slack 渠道 — Events API + Webhook 模式
//!
//! # 工作原理
//!
//! 1. Slack 将消息事件以 HTTP POST 发送到本渠道 Webhook 地址
//! 2. 本渠道验证 HMAC-SHA256 签名（X-Slack-Signature），解析消息
//! 3. `send()` 通过 Slack `chat.postMessage` API 回复
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

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

    async fn post_message(&self, channel_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/chat.postMessage", SLACK_API);
        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&json!({
                "channel": channel_id,
                "text": text,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Slack postMessage request failed: {}", e))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("Slack postMessage response parse failed: {}", e))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = body
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
        let channel_id = &msg.target_session_id;
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        let state = SlackState {
            base: Arc::clone(&self.base),
            bot_token: self.bot_token.clone(),
            signing_secret: self.signing_secret.clone(),
            bus: Arc::new(crate::DummyBus),
            http_client: self.http_client.clone(),
        };
        state.post_message(channel_id, &content).await
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

                let mut metadata: HashMap<String, Value> = HashMap::new();
                metadata.insert("ts".to_string(), Value::String(ts));
                metadata.insert(
                    "team_id".to_string(),
                    Value::String(payload.team_id.clone().unwrap_or_default()),
                );

                // session_id = channel_id（send() 方法调用 chat.postMessage 用）
                state
                    .base
                    .handle_message(
                        &state.bus,
                        &sender_id,
                        &sender_id,
                        &channel_id,
                        text.trim(),
                        metadata,
                    )
                    .await;
            }
        }
    }

    (StatusCode::OK, Json(json!({})))
}
