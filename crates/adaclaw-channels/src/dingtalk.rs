//! 钉钉（DingTalk）渠道 — Outgoing Webhook 模式
//!
//! # 工作原理
//!
//! 1. 钉钉将用户消息以 HTTP POST 发送到本渠道的 Webhook 地址
//! 2. 本渠道验证 HMAC-SHA256 签名，解析消息，发布到 MessageBus
//! 3. Agent 完成后，`send()` 方法 POST 回复到消息中携带的 `sessionWebhook` URL
//!
//! # 配置示例
//!
//! ```toml
//! [channels.dingtalk]
//! kind = "dingtalk"
//! webhook_secret = "SEC..."          # 安全密钥（签名验证用）
//! allow_from = []                    # 空 = 不限制发送者
//!
//! [channels.dingtalk.extra]
//! webhook_port = "9001"              # 本地监听端口
//! webhook_path = "/webhook/dingtalk" # Webhook 路径（需与钉钉后台配置一致）
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
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

// ── 钉钉消息结构 ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DingTalkInbound {
    #[serde(default)]
    msg_type: String,
    text: Option<DingTalkText>,
    #[serde(default)]
    sender_id: String,
    #[serde(default)]
    sender_nick: String,
    #[serde(default)]
    session_webhook: String,
    #[serde(default)]
    conversation_type: String,
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    conversation_title: String,
}

#[derive(Debug, Deserialize, Clone)]
struct DingTalkText {
    content: String,
}

#[derive(Debug, Serialize)]
struct DingTalkReply {
    msgtype: String,
    text: DingTalkReplyText,
}

#[derive(Debug, Serialize)]
struct DingTalkReplyText {
    content: String,
}

// ── 共享状态（传给 axum handler） ────────────────────────────────────────────

#[derive(Clone)]
struct DingTalkState {
    base: Arc<BaseChannel>,
    webhook_secret: Option<String>,
    bus: Arc<dyn MessageBus>,
    #[allow(dead_code)]
    http_client: reqwest::Client,
}

// ── DingTalkChannel ───────────────────────────────────────────────────────────

pub struct DingTalkChannel {
    base: Arc<BaseChannel>,
    webhook_secret: Option<String>,
    webhook_port: u16,
    webhook_path: String,
    http_client: reqwest::Client,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl DingTalkChannel {
    pub fn new(
        webhook_secret: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
    ) -> Self {
        let base = Arc::new(
            BaseChannel::new("dingtalk").with_allow_from(allow_from),
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            webhook_secret,
            webhook_port,
            webhook_path,
            http_client: client,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// 验证钉钉 Outgoing Webhook 签名（备用，实际验证由 axum handler 内联执行）
    ///
    /// sign = base64(HMAC-SHA256(timestamp + "\n" + secret, secret))
    #[allow(dead_code)]
    fn verify_signature(
        &self,
        timestamp: &str,
        sign: &str,
    ) -> bool {
        let secret = match &self.webhook_secret {
            Some(s) => s,
            None => return true, // 未配置密钥，跳过验证
        };

        let message = format!("{}\n{}", timestamp, secret);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(message.as_bytes());
        let result = mac.finalize().into_bytes();
        let expected = base64::engine::general_purpose::STANDARD.encode(result);

        expected == sign
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = DingTalkState {
            base: Arc::clone(&self.base),
            webhook_secret: self.webhook_secret.clone(),
            bus,
            http_client: self.http_client.clone(),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, post(handle_dingtalk_webhook))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "dingtalk",
            addr = %addr,
            path = %self.webhook_path,
            "DingTalk webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async { rx.await.ok(); })
            .await
            .map_err(|e| anyhow!("DingTalk HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "dingtalk", "DingTalk channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let webhook_url = &msg.target_session_id;
        if webhook_url.is_empty() || !webhook_url.starts_with("http") {
            return Err(anyhow!(
                "DingTalk: invalid session_id (expected sessionWebhook URL): '{}'",
                webhook_url
            ));
        }

        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()), // 非文本消息暂不支持
        };

        let reply = DingTalkReply {
            msgtype: "text".to_string(),
            text: DingTalkReplyText { content },
        };

        let resp = self
            .http_client
            .post(webhook_url)
            .json(&reply)
            .send()
            .await
            .map_err(|e| anyhow!("DingTalk reply POST failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "DingTalk reply failed: HTTP {} — {}",
                status,
                body
            ));
        }

        debug!(channel = "dingtalk", "Reply sent successfully");
        Ok(())
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

async fn handle_dingtalk_webhook(
    State(state): State<DingTalkState>,
    headers: HeaderMap,
    Json(body): Json<DingTalkInbound>,
) -> StatusCode {
    // 签名验证
    if let Some(secret) = &state.webhook_secret {
        let timestamp = headers
            .get("timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let sign = headers
            .get("sign")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let message = format!("{}\n{}", timestamp, secret);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(message.as_bytes());
        let expected =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        if expected != sign {
            warn!(channel = "dingtalk", "Signature verification failed");
            return StatusCode::FORBIDDEN;
        }
    }

    // 仅处理 text 类型
    if body.msg_type != "text" {
        debug!(channel = "dingtalk", msg_type = %body.msg_type, "Non-text message, skipping");
        return StatusCode::OK;
    }

    let content = match &body.text {
        Some(t) => t.content.trim().to_string(),
        None => {
            warn!(channel = "dingtalk", "Text message with empty content");
            return StatusCode::OK;
        }
    };

    if content.is_empty() {
        return StatusCode::OK;
    }

    // session_id = sessionWebhook URL（out-of-band 回复时使用）
    let session_id = body.session_webhook.clone();
    let sender_id = body.sender_id.clone();
    let sender_name = body.sender_nick.clone();

    let mut metadata: HashMap<String, Value> = HashMap::new();
    metadata.insert(
        "conversation_id".to_string(),
        Value::String(body.conversation_id.clone()),
    );
    metadata.insert(
        "conversation_type".to_string(),
        Value::String(body.conversation_type.clone()),
    );
    metadata.insert(
        "conversation_title".to_string(),
        Value::String(body.conversation_title.clone()),
    );
    metadata.insert(
        "session_webhook".to_string(),
        Value::String(body.session_webhook.clone()),
    );

    debug!(
        channel = "dingtalk",
        sender_id = %sender_id,
        preview = %content.chars().take(60).collect::<String>(),
        "DingTalk message received"
    );

    state
        .base
        .handle_message(&state.bus, &sender_id, &sender_name, &session_id, &content, metadata)
        .await;

    StatusCode::OK
}
