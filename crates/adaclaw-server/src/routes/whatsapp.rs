//! WhatsApp Cloud API 路由处理器
//!
//! 当 WhatsApp 需要与主 Gateway 服务器共享同一 HTTPS 端口时使用本模块。
//! 这在配合 Cloudflare/ngrok 隧道时很有用——无需再开放额外端口。
//!
//! # 使用方式
//!
//! 在 `server.rs` 中启用：
//! ```rust
//! let app = build_router(Some(whatsapp_state));
//! ```
//!
//! 或使用独立 WhatsApp 端口（默认，与现有 Slack/DingTalk 模式一致）：
//! 在 `channels.whatsapp.extra.webhook_port` 配置独立端口，无需此模块。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use adaclaw_core::channel::{InboundMessage, MessageBus, MessageContent};
use uuid::Uuid;

// ── WhatsApp 路由状态 ─────────────────────────────────────────────────────────

/// WhatsApp 服务器路由共享状态
///
/// 通过 `Router::with_state(state)` 挂载到主服务器。
#[derive(Clone)]
pub struct WhatsAppRouteState {
    /// Graph API Access Token（用于发消息，本路由不直接发消息，仅接收）
    pub access_token: String,
    /// Webhook 验证令牌
    pub verify_token: String,
    /// App Secret（用于 X-Hub-Signature-256 HMAC 验证，可选）
    pub app_secret: Option<String>,
    /// 消息总线（用于将入站消息发布给 Agent）
    pub bus: Arc<dyn MessageBus>,
    /// 允许的手机号白名单（空 = 放行所有人）
    pub allow_from: Vec<String>,
}

impl WhatsAppRouteState {
    /// 验证 X-Hub-Signature-256 HMAC 签名
    fn verify_signature(&self, headers: &HeaderMap, body: &str) -> bool {
        let secret = match &self.app_secret {
            Some(s) => s,
            None => return true,
        };

        let received = headers
            .get("X-Hub-Signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if received.is_empty() {
            warn!(route = "whatsapp", "Missing X-Hub-Signature-256");
            return false;
        }

        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key any size");
        mac.update(body.as_bytes());
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        // 常量时间比较
        let a = expected.as_bytes();
        let b = received.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    fn is_allowed(&self, sender: &str) -> bool {
        if self.allow_from.is_empty() {
            return true;
        }
        self.allow_from.iter().any(|a| a == sender)
    }
}

// ── Query 参数 ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    #[serde(rename = "hub.mode")]
    pub hub_mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub hub_verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub hub_challenge: Option<String>,
}

// ── 路由处理器 ────────────────────────────────────────────────────────────────

/// `GET /whatsapp` — Meta Webhook 订阅验证
pub async fn whatsapp_verify(
    State(state): State<WhatsAppRouteState>,
    Query(params): Query<VerifyQuery>,
) -> (StatusCode, String) {
    let mode = params.hub_mode.as_deref().unwrap_or("");
    let token = params.hub_verify_token.as_deref().unwrap_or("");
    let challenge = params.hub_challenge.as_deref().unwrap_or("");

    if mode == "subscribe" && token == state.verify_token {
        info!(route = "whatsapp", "Webhook verification successful");
        (StatusCode::OK, challenge.to_string())
    } else {
        warn!(
            route = "whatsapp",
            mode = %mode,
            "Webhook verification failed"
        );
        (StatusCode::FORBIDDEN, "Forbidden".to_string())
    }
}

/// `POST /whatsapp` — 接收 Meta 推送的消息事件
pub async fn whatsapp_receive(
    State(state): State<WhatsAppRouteState>,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    if !state.verify_signature(&headers, &body) {
        warn!(route = "whatsapp", "Signature verification failed");
        return StatusCode::FORBIDDEN;
    }

    let payload: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(route = "whatsapp", error = %e, "Failed to parse payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.get("object").and_then(|v| v.as_str())
        != Some("whatsapp_business_account")
    {
        return StatusCode::OK;
    }

    let entries = match payload.get("entry").and_then(|v| v.as_array()) {
        Some(e) => e.clone(),
        None => return StatusCode::OK,
    };

    for entry in &entries {
        let changes = match entry.get("changes").and_then(|v| v.as_array()) {
            Some(c) => c.clone(),
            None => continue,
        };

        for change in &changes {
            if change.get("field").and_then(|v| v.as_str()) != Some("messages") {
                continue;
            }

            let value = match change.get("value") {
                Some(v) => v,
                None => continue,
            };

            if value
                .get("messaging_product")
                .and_then(|v| v.as_str())
                != Some("whatsapp")
            {
                continue;
            }

            // 联系人名称映射
            let mut names: HashMap<String, String> = HashMap::new();
            if let Some(contacts) = value.get("contacts").and_then(|v| v.as_array()) {
                for c in contacts {
                    let wa_id = c.get("wa_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = c
                        .get("profile")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !wa_id.is_empty() && !name.is_empty() {
                        names.insert(wa_id.to_string(), name.to_string());
                    }
                }
            }

            let msgs = match value.get("messages").and_then(|v| v.as_array()) {
                Some(m) => m.clone(),
                None => continue,
            };

            for msg in &msgs {
                let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("");
                if from.is_empty() {
                    continue;
                }

                let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let text = match msg_type {
                    "text" => msg
                        .get("text")
                        .and_then(|t| t.get("body"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => continue, // 只处理文本消息
                };

                if text.trim().is_empty() {
                    continue;
                }

                if !state.is_allowed(from) {
                    warn!(
                        route = "whatsapp",
                        from = %from,
                        "Sender not in allowlist"
                    );
                    continue;
                }

                let sender_name = names.get(from).cloned().unwrap_or_else(|| from.to_string());
                let msg_id = msg
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let inbound = InboundMessage {
                    id: Uuid::new_v4(),
                    channel: "whatsapp".to_string(),
                    session_id: from.to_string(),
                    sender_id: from.to_string(),
                    sender_name,
                    content: MessageContent::Text(text.trim().to_string()),
                    reply_to: None,
                    metadata: {
                        let mut m = HashMap::new();
                        m.insert(
                            "message_id".to_string(),
                            serde_json::Value::String(msg_id),
                        );
                        m
                    },
                };

                if let Err(e) = state.bus.send_inbound(inbound).await {
                    warn!(route = "whatsapp", error = %e, "Failed to publish to bus");
                }
            }
        }
    }

    StatusCode::OK
}
