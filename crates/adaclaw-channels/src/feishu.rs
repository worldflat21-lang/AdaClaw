//! 飞书（Feishu / Lark）渠道 — 事件订阅 Webhook 模式
//!
//! # 工作原理
//!
//! 1. 飞书将 im.message.receive_v1 事件以 HTTP POST 发送到本渠道 Webhook 地址
//! 2. 本渠道验证 `verification_token`，解析消息，发布到 MessageBus
//! 3. Agent 完成后，`send()` 通过飞书 Open API 回复消息
//!    （需要 app_id + app_secret 换取 tenant_access_token）
//!
//! # 配置示例
//!
//! ```toml
//! [channels.feishu]
//! kind = "feishu"
//! allow_from = []
//!
//! [channels.feishu.extra]
//! webhook_port = "9002"
//! webhook_path = "/webhook/feishu"
//! app_id = "cli_xxx"
//! app_secret = "yyy"
//! verification_token = "zzz"    # 飞书后台 → 事件订阅 → Verification Token
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info, warn};

// ── 飞书事件结构（schema 2.0）──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FeishuEvent {
    schema: Option<String>,
    header: Option<FeishuHeader>,
    event: Option<Value>,
    // URL 验证挑战（旧格式）
    challenge: Option<String>,
    token: Option<String>,
    #[serde(rename = "type")]
    event_type_legacy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FeishuHeader {
    event_type: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Serialize)]
struct FeishuChallengeResponse {
    challenge: String,
}

// ── 飞书 Access Token（带缓存）──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct TokenCache {
    token: String,
    expires_at: u64,
}

// ── 共享状态 ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct FeishuState {
    base: Arc<BaseChannel>,
    verification_token: Option<String>,
    bus: Arc<dyn MessageBus>,
    app_id: String,
    app_secret: String,
    http_client: reqwest::Client,
    token_cache: Arc<Mutex<Option<TokenCache>>>,
}

impl FeishuState {
    /// 获取 tenant_access_token（本地缓存 + 自动刷新）
    async fn get_access_token(&self) -> Result<String> {
        if self.app_id.is_empty() || self.app_secret.is_empty() {
            return Err(anyhow!("Feishu app_id or app_secret not configured"));
        }

        // 检查缓存
        {
            let cache = self.token_cache.lock().await;
            if let Some(ref tc) = *cache {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // 提前 60 秒刷新
                if tc.expires_at > now + 60 {
                    return Ok(tc.token.clone());
                }
            }
        }

        // 刷新 token
        let resp = self
            .http_client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Feishu token request failed: {}", e))?;

        #[derive(Deserialize)]
        struct TokenResp {
            code: i64,
            msg: String,
            tenant_access_token: Option<String>,
            expire: Option<u64>,
        }
        let body: TokenResp = resp
            .json()
            .await
            .map_err(|e| anyhow!("Feishu token response parse failed: {}", e))?;

        if body.code != 0 {
            return Err(anyhow!(
                "Feishu token error {}: {}",
                body.code,
                body.msg
            ));
        }

        let token = body
            .tenant_access_token
            .ok_or_else(|| anyhow!("Feishu token response missing tenant_access_token"))?;
        let expire_secs = body.expire.unwrap_or(7200);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // 更新缓存
        let mut cache = self.token_cache.lock().await;
        *cache = Some(TokenCache {
            token: token.clone(),
            expires_at: now + expire_secs,
        });

        debug!(channel = "feishu", "Access token refreshed");
        Ok(token)
    }

    /// 通过 Feishu Open API 发送消息
    async fn send_message(&self, chat_id: &str, content: &str) -> Result<()> {
        let token = self.get_access_token().await?;

        // 飞书消息内容需要 JSON 字符串化
        let msg_content = serde_json::to_string(&json!({ "text": content }))
            .unwrap_or_default();

        let url = "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id";
        let resp = self
            .http_client
            .post(url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": msg_content,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Feishu sendMessage request failed: {}", e))?;

        #[derive(Deserialize)]
        struct SendResp {
            code: i64,
            msg: String,
        }
        let body: SendResp = resp
            .json()
            .await
            .map_err(|e| anyhow!("Feishu sendMessage response parse failed: {}", e))?;

        if body.code != 0 {
            return Err(anyhow!(
                "Feishu sendMessage error {}: {}",
                body.code,
                body.msg
            ));
        }

        debug!(channel = "feishu", chat_id = %chat_id, "Message sent successfully");
        Ok(())
    }
}

// ── FeishuChannel ─────────────────────────────────────────────────────────────

pub struct FeishuChannel {
    base: Arc<BaseChannel>,
    verification_token: Option<String>,
    app_id: String,
    app_secret: String,
    webhook_port: u16,
    webhook_path: String,
    http_client: reqwest::Client,
    token_cache: Arc<Mutex<Option<TokenCache>>>,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl FeishuChannel {
    pub fn new(
        app_id: String,
        app_secret: String,
        verification_token: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
    ) -> Self {
        let base = Arc::new(
            BaseChannel::new("feishu").with_allow_from(allow_from),
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            verification_token,
            app_id,
            app_secret,
            webhook_port,
            webhook_path,
            http_client: client,
            token_cache: Arc::new(Mutex::new(None)),
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = FeishuState {
            base: Arc::clone(&self.base),
            verification_token: self.verification_token.clone(),
            bus,
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
            http_client: self.http_client.clone(),
            token_cache: Arc::clone(&self.token_cache),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, post(handle_feishu_event))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "feishu",
            addr = %addr,
            path = %self.webhook_path,
            "Feishu webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async { rx.await.ok(); })
            .await
            .map_err(|e| anyhow!("Feishu HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "feishu", "Feishu channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let chat_id = &msg.target_session_id;
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        let state = FeishuState {
            base: Arc::clone(&self.base),
            verification_token: self.verification_token.clone(),
            bus: Arc::new(crate::DummyBus),
            app_id: self.app_id.clone(),
            app_secret: self.app_secret.clone(),
            http_client: self.http_client.clone(),
            token_cache: Arc::clone(&self.token_cache),
        };

        state.send_message(chat_id, &content).await
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

async fn handle_feishu_event(
    State(state): State<FeishuState>,
    Json(body): Json<FeishuEvent>,
) -> (StatusCode, Json<Value>) {
    // ── URL 验证（旧格式）──────────────────────────────────────────────────────
    if body.event_type_legacy.as_deref() == Some("url_verification") {
        if let Some(challenge) = &body.challenge {
            return (
                StatusCode::OK,
                Json(json!({ "challenge": challenge })),
            );
        }
    }

    // ── URL 验证（schema 2.0）─────────────────────────────────────────────────
    if let Some(header) = &body.header {
        if header.event_type.as_deref() == Some("url_verification") {
            if let Some(event) = &body.event {
                if let Some(challenge) = event.get("challenge").and_then(|v| v.as_str()) {
                    return (
                        StatusCode::OK,
                        Json(json!({ "challenge": challenge })),
                    );
                }
            }
        }
    }

    // ── Token 验证 ─────────────────────────────────────────────────────────────
    if let Some(expected) = &state.verification_token {
        let received_token = body
            .header
            .as_ref()
            .and_then(|h| h.token.as_deref())
            .or(body.token.as_deref())
            .unwrap_or("");
        if received_token != expected {
            warn!(channel = "feishu", "Verification token mismatch");
            return (StatusCode::FORBIDDEN, Json(json!({})));
        }
    }

    // ── 处理 im.message.receive_v1 事件 ──────────────────────────────────────
    let event_type = body
        .header
        .as_ref()
        .and_then(|h| h.event_type.as_deref())
        .unwrap_or("");

    if event_type != "im.message.receive_v1" {
        debug!(
            channel = "feishu",
            event_type = %event_type,
            "Non-message event, skipping"
        );
        return (StatusCode::OK, Json(json!({})));
    }

    let event = match &body.event {
        Some(e) => e,
        None => return (StatusCode::OK, Json(json!({}))),
    };

    // 提取发送者 ID
    let sender_id = event
        .pointer("/sender/sender_id/open_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let sender_user_id = event
        .pointer("/sender/sender_id/user_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&sender_id)
        .to_string();

    // 提取消息信息
    let message = match event.get("message") {
        Some(m) => m,
        None => return (StatusCode::OK, Json(json!({}))),
    };

    let msg_type = message
        .get("message_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // 仅处理 text 类型
    if msg_type != "text" {
        debug!(
            channel = "feishu",
            msg_type = %msg_type,
            "Non-text message, skipping"
        );
        return (StatusCode::OK, Json(json!({})));
    }

    // 飞书消息内容是 JSON 字符串，需要二次解析
    let content_str = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    let content_json: Value = serde_json::from_str(content_str).unwrap_or(json!({}));
    let text = content_json
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if text.is_empty() {
        return (StatusCode::OK, Json(json!({})));
    }

    let chat_id = message
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let chat_type = message
        .get("chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("p2p")
        .to_string();

    let message_id = message
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    debug!(
        channel = "feishu",
        sender_id = %sender_id,
        chat_id = %chat_id,
        preview = %text.chars().take(60).collect::<String>(),
        "Feishu message received"
    );

    let mut metadata: HashMap<String, Value> = HashMap::new();
    metadata.insert("message_id".to_string(), Value::String(message_id));
    metadata.insert("chat_type".to_string(), Value::String(chat_type));
    metadata.insert("user_id".to_string(), Value::String(sender_user_id));

    // session_id = chat_id（send() 方法用此值调用 Feishu API）
    state
        .base
        .handle_message(&state.bus, &sender_id, &sender_id, &chat_id, &text, metadata)
        .await;

    (StatusCode::OK, Json(json!({})))
}
