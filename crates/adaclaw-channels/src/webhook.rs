//! 通用 HTTP Webhook 渠道
//!
//! # 工作原理
//!
//! 接收来自任意系统的 HTTP POST 请求，转化为 Agent 消息。
//! 适用于自定义集成（CI/CD 通知、监控告警、业务系统等）。
//!
//! # 入站请求格式（JSON）
//!
//! ```json
//! {
//!   "sender_id": "system-alert",
//!   "sender_name": "PagerDuty",
//!   "content": "CPU 使用率超过 90%",
//!   "session_id": "ops-channel"
//! }
//! ```
//!
//! # 出站回复（可选）
//!
//! 若配置 `outbound_url`，Agent 回复将 POST 到该 URL：
//! ```json
//! { "session_id": "ops-channel", "content": "Agent 回复内容" }
//! ```
//!
//! # 配置示例
//!
//! ```toml
//! [channels.webhook]
//! kind = "webhook"
//! webhook_secret = "my-secret"    # HMAC-SHA256 验证（可选）
//! allow_from = []
//!
//! [channels.webhook.extra]
//! webhook_port = "9005"
//! webhook_path = "/webhook/custom"
//! outbound_url = "https://your-system.com/agent-reply"  # 可选
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
use tracing::{debug, info, warn};

// ── 请求/响应结构 ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WebhookInbound {
    #[serde(default = "default_sender_id")]
    sender_id: String,
    #[serde(default = "default_sender_name")]
    sender_name: String,
    content: String,
    #[serde(default = "default_session_id")]
    session_id: String,
    #[serde(default)]
    metadata: HashMap<String, Value>,
}

fn default_sender_id() -> String {
    "webhook_user".to_string()
}
fn default_sender_name() -> String {
    "Webhook".to_string()
}
fn default_session_id() -> String {
    "webhook:default".to_string()
}

#[derive(Debug, Serialize)]
struct WebhookOutbound {
    session_id: String,
    content: String,
}

// ── 共享状态 ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct WebhookState {
    base: Arc<BaseChannel>,
    hmac_secret: Option<String>,
    bus: Arc<dyn MessageBus>,
    #[allow(dead_code)]
    http_client: reqwest::Client,
    #[allow(dead_code)]
    outbound_url: Option<String>,
}

impl WebhookState {
    /// 验证 HMAC-SHA256 签名
    /// Header: X-Webhook-Signature: sha256={hex}
    fn verify_signature(&self, headers: &HeaderMap, body: &str) -> bool {
        let secret = match &self.hmac_secret {
            Some(s) => s,
            None => return true, // 未配置密钥，跳过验证
        };

        let sig_header = headers
            .get("X-Webhook-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let received = sig_header.trim_start_matches("sha256=");
        if received.is_empty() {
            return false;
        }

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(body.as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());

        expected == received
    }
}

// ── WebhookChannel ────────────────────────────────────────────────────────────

pub struct WebhookChannel {
    base: Arc<BaseChannel>,
    hmac_secret: Option<String>,
    webhook_port: u16,
    webhook_path: String,
    outbound_url: Option<String>,
    http_client: reqwest::Client,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl WebhookChannel {
    pub fn new(
        hmac_secret: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
        outbound_url: Option<String>,
    ) -> Self {
        let base = Arc::new(BaseChannel::new("webhook").with_allow_from(allow_from));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            hmac_secret,
            webhook_port,
            webhook_path,
            outbound_url,
            http_client: client,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Channel for WebhookChannel {
    fn name(&self) -> &str {
        "webhook"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = WebhookState {
            base: Arc::clone(&self.base),
            hmac_secret: self.hmac_secret.clone(),
            bus,
            http_client: self.http_client.clone(),
            outbound_url: self.outbound_url.clone(),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, post(handle_webhook))
            .route("/health/webhook", axum::routing::get(|| async { StatusCode::OK }))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "webhook",
            addr = %addr,
            path = %self.webhook_path,
            "Generic webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async { rx.await.ok(); })
            .await
            .map_err(|e| anyhow!("Webhook HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "webhook", "Webhook channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let outbound_url = match &self.outbound_url {
            Some(u) => u.clone(),
            None => {
                debug!(channel = "webhook", "No outbound_url configured, skipping reply");
                return Ok(());
            }
        };

        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        let payload = WebhookOutbound {
            session_id: msg.target_session_id.clone(),
            content,
        };

        let resp = self
            .http_client
            .post(&outbound_url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow!("Webhook outbound POST failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Webhook outbound failed: HTTP {} — {}",
                status,
                body
            ));
        }

        debug!(channel = "webhook", "Outbound reply sent successfully");
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

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    // HMAC 签名验证
    if !state.verify_signature(&headers, &body) {
        warn!(channel = "webhook", "Signature verification failed");
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Invalid signature" })));
    }

    let inbound: WebhookInbound = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(channel = "webhook", error = %e, "Failed to parse webhook body");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON: {}", e) })),
            );
        }
    };

    if inbound.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "content is required" })),
        );
    }

    debug!(
        channel = "webhook",
        sender_id = %inbound.sender_id,
        preview = %inbound.content.chars().take(60).collect::<String>(),
        "Webhook message received"
    );

    let mut metadata = inbound.metadata;
    metadata.insert("source".to_string(), Value::String("webhook".to_string()));

    state
        .base
        .handle_message(
            &state.bus,
            &inbound.sender_id,
            &inbound.sender_name,
            &inbound.session_id,
            inbound.content.trim(),
            metadata,
        )
        .await;

    (StatusCode::OK, Json(json!({ "status": "accepted" })))
}
