//! WhatsApp Business Cloud API 渠道
//!
//! # 工作原理
//!
//! 1. Meta 发送 GET 到本渠道 Webhook 地址进行验证（hub.mode / hub.verify_token / hub.challenge）
//! 2. Meta 发送 POST 传递消息，使用 `X-Hub-Signature-256` HMAC 验证
//! 3. `send()` 通过 Graph API 发送消息
//!
//! # 配置示例
//!
//! ```toml
//! [channels.whatsapp]
//! kind = "whatsapp"
//! token = "EAA..."              # WhatsApp Cloud API Access Token
//! webhook_secret = "..."        # App Secret（可选，用于 HMAC 验证）
//! allow_from = ["1234567890"]   # 允许的手机号（+号可选）
//!
//! [channels.whatsapp.extra]
//! phone_number_id = "12345678"  # Phone Number ID（Meta App Dashboard 中获取）
//! verify_token = "my_token"     # Webhook 验证令牌（自定义）
//! webhook_port = "9005"
//! webhook_path = "/whatsapp"
//! ```
//!
//! # 部署要求
//!
//! WhatsApp Cloud API Webhook 必须使用 HTTPS。请配合隧道使用：
//! ```toml
//! [tunnel]
//! provider = "cloudflare"   # 或 ngrok / tailscale
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

const GRAPH_API_BASE: &str = "https://graph.facebook.com/v18.0";

// ── 入站消息解析 ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WhatsAppWebhookPayload {
    #[serde(rename = "object")]
    object: Option<String>,
    entry: Option<Vec<WhatsAppEntry>>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppEntry {
    changes: Option<Vec<WhatsAppChange>>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppChange {
    value: Option<WhatsAppValue>,
    field: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppValue {
    messaging_product: Option<String>,
    contacts: Option<Vec<WhatsAppContact>>,
    messages: Option<Vec<WhatsAppMessage>>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppContact {
    profile: Option<WhatsAppProfile>,
    wa_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppProfile {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppMessage {
    from: Option<String>,
    id: Option<String>,
    text: Option<WhatsAppText>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    image: Option<WhatsAppMedia>,
    audio: Option<WhatsAppMedia>,
    document: Option<WhatsAppMedia>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppText {
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhatsAppMedia {
    id: Option<String>,
    #[allow(dead_code)]
    mime_type: Option<String>,
    caption: Option<String>,
}

/// Meta Webhook 验证请求参数
#[derive(Debug, Deserialize)]
pub struct VerifyParams {
    #[serde(rename = "hub.mode")]
    pub hub_mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub hub_verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub hub_challenge: Option<String>,
}

// ── 共享状态 ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WhatsAppState {
    pub base: Arc<BaseChannel>,
    pub access_token: String,
    pub phone_number_id: String,
    pub verify_token: String,
    /// App Secret，用于 X-Hub-Signature-256 HMAC 验证（可选）
    pub app_secret: Option<String>,
    pub bus: Arc<dyn MessageBus>,
    pub http_client: reqwest::Client,
}

impl WhatsAppState {
    /// 验证 Meta 的 HMAC-SHA256 签名
    /// X-Hub-Signature-256: sha256=<HMAC_SHA256(app_secret, body)>
    pub fn verify_signature(&self, headers: &HeaderMap, body: &str) -> bool {
        let secret = match &self.app_secret {
            Some(s) => s,
            None => return true, // 未配置 App Secret，跳过验证
        };

        let received_sig = headers
            .get("X-Hub-Signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if received_sig.is_empty() {
            warn!(channel = "whatsapp", "Missing X-Hub-Signature-256 header");
            return false;
        }

        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(body.as_bytes());
        let result = mac.finalize().into_bytes();
        let expected = format!("sha256={}", hex::encode(result));

        constant_time_eq(expected.as_bytes(), received_sig.as_bytes())
    }

    /// 发送文本消息到 WhatsApp 用户
    pub async fn send_text(&self, to: &str, text: &str) -> Result<()> {
        let url = format!("{}/{}/messages", GRAPH_API_BASE, self.phone_number_id);
        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .json(&json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "text",
                "text": { "body": text }
            }))
            .send()
            .await
            .map_err(|e| anyhow!("WhatsApp send request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            return Err(anyhow!("WhatsApp Graph API error {}: {}", status, body));
        }
        Ok(())
    }
}

/// 常量时间字节串比较（防时序攻击）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── WhatsAppChannel ───────────────────────────────────────────────────────────

/// WhatsApp Business Cloud API 渠道
///
/// 配置字段（从 `ChannelConfig` 读取）：
/// - `token` → `access_token`（Graph API Access Token）
/// - `webhook_secret` → `app_secret`（可选，用于 HMAC 验证）
/// - `extra.phone_number_id` → Phone Number ID
/// - `extra.verify_token` → Webhook 验证令牌（自定义字符串）
/// - `extra.webhook_port` → 内嵌 HTTP 服务器端口（默认 9005）
/// - `extra.webhook_path` → Webhook 路径（默认 /whatsapp）
/// - `allow_from` → 允许的手机号白名单（空 = 放行所有人）
pub struct WhatsAppChannel {
    base: Arc<BaseChannel>,
    access_token: String,
    phone_number_id: String,
    verify_token: String,
    app_secret: Option<String>,
    webhook_port: u16,
    webhook_path: String,
    http_client: reqwest::Client,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl WhatsAppChannel {
    pub fn new(
        access_token: String,
        phone_number_id: String,
        verify_token: String,
        app_secret: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
    ) -> Self {
        let base = Arc::new(BaseChannel::new("whatsapp").with_allow_from(allow_from));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            access_token,
            phone_number_id,
            verify_token,
            app_secret,
            webhook_port,
            webhook_path,
            http_client: client,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// 从 `ChannelConfig.extra` 等字段构建 `WhatsAppChannel`。
    /// 供 `run.rs` 的渠道工厂调用。
    pub fn from_extra(
        token: Option<String>,
        webhook_secret: Option<String>,
        allow_from: Vec<String>,
        extra: &HashMap<String, String>,
    ) -> Result<Self> {
        let access_token =
            token.ok_or_else(|| anyhow!("channels.whatsapp.token (access_token) is required"))?;
        let phone_number_id = extra
            .get("phone_number_id")
            .cloned()
            .ok_or_else(|| anyhow!("channels.whatsapp.extra.phone_number_id is required"))?;
        let verify_token = extra
            .get("verify_token")
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let webhook_port = extra
            .get("webhook_port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(9005u16);
        let webhook_path = extra
            .get("webhook_path")
            .cloned()
            .unwrap_or_else(|| "/whatsapp".to_string());

        Ok(Self::new(
            access_token,
            phone_number_id,
            verify_token,
            webhook_secret,
            allow_from,
            webhook_port,
            webhook_path,
        ))
    }
}

#[async_trait]
impl Channel for WhatsAppChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = WhatsAppState {
            base: Arc::clone(&self.base),
            access_token: self.access_token.clone(),
            phone_number_id: self.phone_number_id.clone(),
            verify_token: self.verify_token.clone(),
            app_secret: self.app_secret.clone(),
            bus,
            http_client: self.http_client.clone(),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, get(handle_whatsapp_verify))
            .route(&path, post(handle_whatsapp_message))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "whatsapp",
            addr = %addr,
            path = %self.webhook_path,
            "WhatsApp Cloud API webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                rx.await.ok();
            })
            .await
            .map_err(|e| anyhow!("WhatsApp HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "whatsapp", "WhatsApp channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let to = &msg.target_session_id;
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()), // WhatsApp 暂只支持文本
        };

        let state = WhatsAppState {
            base: Arc::clone(&self.base),
            access_token: self.access_token.clone(),
            phone_number_id: self.phone_number_id.clone(),
            verify_token: self.verify_token.clone(),
            app_secret: self.app_secret.clone(),
            bus: Arc::new(crate::DummyBus),
            http_client: self.http_client.clone(),
        };
        state.send_text(to, &content).await
    }

    async fn stop(&self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.base.set_running(false);
        Ok(())
    }
}

// ── Axum Handlers ─────────────────────────────────────────────────────────────

/// GET /whatsapp — Meta Webhook 验证
///
/// Meta 在注册 Webhook 时发送此请求，验证令牌匹配后回应 challenge。
pub async fn handle_whatsapp_verify(
    State(state): State<WhatsAppState>,
    Query(params): Query<VerifyParams>,
) -> (StatusCode, String) {
    let mode = params.hub_mode.as_deref().unwrap_or("");
    let token = params.hub_verify_token.as_deref().unwrap_or("");
    let challenge = params.hub_challenge.as_deref().unwrap_or("");

    if mode == "subscribe" && constant_time_eq(token.as_bytes(), state.verify_token.as_bytes()) {
        info!(channel = "whatsapp", "Webhook verification successful");
        (StatusCode::OK, challenge.to_string())
    } else {
        warn!(
            channel = "whatsapp",
            mode = %mode,
            "Webhook verification failed: token mismatch"
        );
        (StatusCode::FORBIDDEN, "Forbidden".to_string())
    }
}

/// POST /whatsapp — 接收 Meta 推送的消息
pub async fn handle_whatsapp_message(
    State(state): State<WhatsAppState>,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    // HMAC 签名验证
    if !state.verify_signature(&headers, &body) {
        warn!(
            channel = "whatsapp",
            "X-Hub-Signature-256 verification failed"
        );
        return StatusCode::FORBIDDEN;
    }

    let payload: WhatsAppWebhookPayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(channel = "whatsapp", error = %e, "Failed to parse WhatsApp payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    // 验证是否为 WhatsApp Business Account 事件
    if payload.object.as_deref() != Some("whatsapp_business_account") {
        return StatusCode::OK;
    }

    let entries = match payload.entry {
        Some(e) => e,
        None => return StatusCode::OK,
    };

    for entry in entries {
        let changes = match entry.changes {
            Some(c) => c,
            None => continue,
        };
        for change in changes {
            if change.field.as_deref() != Some("messages") {
                continue;
            }
            let value = match change.value {
                Some(v) => v,
                None => continue,
            };

            if value.messaging_product.as_deref() != Some("whatsapp") {
                continue;
            }

            // 构建联系人名称映射 wa_id → display_name
            let mut contact_names: HashMap<String, String> = HashMap::new();
            if let Some(contacts) = &value.contacts {
                for c in contacts {
                    if let (Some(wa_id), Some(profile)) = (&c.wa_id, &c.profile)
                        && let Some(name) = &profile.name
                    {
                        contact_names.insert(wa_id.clone(), name.clone());
                    }
                }
            }

            let messages = match value.messages {
                Some(m) => m,
                None => continue,
            };

            for msg in messages {
                let from = msg.from.clone().unwrap_or_default();
                if from.is_empty() {
                    continue;
                }

                let sender_name = contact_names
                    .get(&from)
                    .cloned()
                    .unwrap_or_else(|| from.clone());

                let (text, msg_type_label) = match msg.msg_type.as_deref() {
                    Some("text") => {
                        let body_text = msg
                            .text
                            .as_ref()
                            .and_then(|t| t.body.clone())
                            .unwrap_or_default();
                        if body_text.trim().is_empty() {
                            continue;
                        }
                        (body_text, "text")
                    }
                    Some("image") => {
                        let caption = msg
                            .image
                            .as_ref()
                            .and_then(|m| m.caption.clone())
                            .unwrap_or_default();
                        let id = msg
                            .image
                            .as_ref()
                            .and_then(|m| m.id.clone())
                            .unwrap_or_default();
                        let text = if caption.is_empty() {
                            format!("[Image: {}]", id)
                        } else {
                            format!("[Image: {}] {}", id, caption)
                        };
                        (text, "image")
                    }
                    Some("audio") => {
                        let id = msg
                            .audio
                            .as_ref()
                            .and_then(|m| m.id.clone())
                            .unwrap_or_default();
                        (format!("[Audio: {}]", id), "audio")
                    }
                    Some("document") => {
                        let id = msg
                            .document
                            .as_ref()
                            .and_then(|m| m.id.clone())
                            .unwrap_or_default();
                        let caption = msg
                            .document
                            .as_ref()
                            .and_then(|m| m.caption.clone())
                            .unwrap_or_default();
                        let text = if caption.is_empty() {
                            format!("[Document: {}]", id)
                        } else {
                            format!("[Document: {}] {}", id, caption)
                        };
                        (text, "document")
                    }
                    _ => continue,
                };

                debug!(
                    channel = "whatsapp",
                    from = %from,
                    sender_name = %sender_name,
                    msg_type = %msg_type_label,
                    preview = %text.chars().take(60).collect::<String>(),
                    "WhatsApp message received"
                );

                let mut metadata: HashMap<String, Value> = HashMap::new();
                if let Some(msg_id) = &msg.id {
                    metadata.insert("message_id".to_string(), Value::String(msg_id.clone()));
                }
                metadata.insert(
                    "msg_type".to_string(),
                    Value::String(msg_type_label.to_string()),
                );

                // session_id = from（手机号），send() 回复时用作 to
                state
                    .base
                    .handle_message(
                        &state.bus,
                        &from,
                        &sender_name,
                        &from,
                        text.trim(),
                        metadata,
                    )
                    .await;
            }
        }
    }

    StatusCode::OK
}
