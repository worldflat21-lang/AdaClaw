//! `POST /v1/chat` — Gateway REST chat 端点
//!
//! 接受 JSON 请求，将消息注入 Agent 消息总线，同步等待回复并返回。
//!
//! ## 请求格式
//! ```json
//! {
//!   "message": "Hello, what can you do?",
//!   "session_id": "optional-session-uuid",
//!   "agent": "optional-agent-id"
//! }
//! ```
//!
//! ## 响应格式
//! ```json
//! { "response": "...", "session_id": "uuid" }
//! ```
//!
//! ## 错误响应
//! ```json
//! { "error": "..." }
//! ```
//!
//! ## 认证
//! 端点由 Bearer Token 中间件保护（见 `middleware::require_auth`）。
//!
//! ## 超时
//! 等待 Agent 响应最长 60 秒，超时返回 503。

use adaclaw_core::channel::{InboundMessage, MessageContent, OutboundMessage};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

// ── 全局消息总线句柄（daemon 启动时注入）─────────────────────────────────────

static INBOUND_TX: OnceLock<mpsc::Sender<InboundMessage>> = OnceLock::new();
static OUTBOUND_TX: OnceLock<broadcast::Sender<OutboundMessage>> = OnceLock::new();

/// 注入 Agent 消息总线（daemon 启动时调用一次）。
///
/// - `inbound_tx`：用于向 Agent bus 发送入站消息
/// - `outbound_tx`：用于订阅 Agent 的出站回复（broadcast）
///
/// 返回 `true` 表示注入成功；`false` 表示已有现有值（OnceLock 不允许覆盖）。
pub fn set_chat_bus(
    inbound_tx: mpsc::Sender<InboundMessage>,
    outbound_tx: broadcast::Sender<OutboundMessage>,
) -> bool {
    let r1 = INBOUND_TX.set(inbound_tx).is_ok();
    let r2 = OUTBOUND_TX.set(outbound_tx).is_ok();
    r1 && r2
}

// ── 请求 / 响应结构体 ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChatRequest {
    /// 用户消息内容
    pub message: String,
    /// 可选会话 ID（不填时自动生成 UUID v4）
    pub session_id: Option<String>,
    /// 可选指定目标 Agent ID（不填时走路由规则）
    #[allow(dead_code)]
    pub agent: Option<String>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub response: String,
    pub session_id: String,
}

// ── 处理器 ────────────────────────────────────────────────────────────────────

/// `POST /v1/chat`
///
/// 将消息注入 Agent bus 并同步等待回复（最长 60 秒）。
pub async fn chat(body: Option<Json<ChatRequest>>) -> Response {
    let req = match body {
        Some(Json(r)) => r,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Request body must be valid JSON with 'message' field"})),
            )
                .into_response();
        }
    };

    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "'message' field cannot be empty"})),
        )
            .into_response();
    }

    // 获取全局总线句柄
    let inbound_tx = match INBOUND_TX.get() {
        Some(tx) => tx,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Agent bus not connected — daemon not running or bus not initialised"})),
            )
                .into_response();
        }
    };

    let outbound_tx = match OUTBOUND_TX.get() {
        Some(tx) => tx,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    json!({"error": "Agent bus not connected — outbound channel not initialised"}),
                ),
            )
                .into_response();
        }
    };

    let session_id = req
        .session_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let session_id_clone = session_id.clone();

    // 订阅出站广播 **在发送入站消息之前**，防止漏接快速响应
    let mut outbound_rx = outbound_tx.subscribe();

    // 构造 InboundMessage
    let msg = InboundMessage {
        id: Uuid::new_v4(),
        channel: "gateway".to_string(),
        session_id: session_id.clone(),
        sender_id: "gateway_api".to_string(),
        sender_name: "Gateway".to_string(),
        content: MessageContent::Text(req.message),
        reply_to: None,
        metadata: HashMap::new(),
    };

    // 发送消息到 bus
    if let Err(e) = inbound_tx.send(msg).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": format!("Failed to send to agent bus: {}", e)})),
        )
            .into_response();
    }

    // 等待匹配 session_id 的出站回复（最长 60 秒）
    const TIMEOUT_SECS: u64 = 60;

    let result = tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), async move {
        loop {
            match outbound_rx.recv().await {
                Ok(out) if out.target_session_id == session_id_clone => {
                    return Ok(out);
                }
                Ok(_) => {
                    // 其他 session 的消息，继续等待
                    continue;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // 广播滞后：已丢弃 n 条旧消息，继续接收
                    tracing::warn!(
                        skipped = n,
                        "Gateway chat: broadcast lagged, {} messages skipped",
                        n
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err("outbound channel closed");
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(out)) => {
            let text = match out.content {
                MessageContent::Text(t) => t,
                MessageContent::Image(_) => "[image response]".to_string(),
                MessageContent::Audio(_) => "[audio response]".to_string(),
                MessageContent::File { name, .. } => format!("[file: {}]", name),
            };
            (
                StatusCode::OK,
                Json(json!({
                    "response": text,
                    "session_id": session_id,
                })),
            )
                .into_response()
        }
        Ok(Err(reason)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": reason})),
        )
            .into_response(),
        Err(_timeout) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({
                "error": format!("Request timed out after {}s — the agent is still processing, retry with the same session_id", TIMEOUT_SECS),
                "session_id": session_id,
            })),
        )
            .into_response(),
    }
}
