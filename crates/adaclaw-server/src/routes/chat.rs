//! `POST /v1/chat` — Gateway REST chat 端点
//!
//! 接受 JSON 请求，将消息注入 Agent 消息总线，同步等待回复并返回。
//!
//! ## 请求格式
//! ```json
//! {
//!   "message": "What's in this image?",
//!   "session_id": "optional-session-uuid",
//!   "agent": "optional-agent-id",
//!   "images": [
//!     { "media_type": "image/png", "data_base64": "iVBORw0KGgo..." }
//!   ]
//! }
//! ```
//! `images` is optional (vision-capable models only) and may contain several.
//! When `images` is present, `message` may be empty.
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
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;
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
    /// 可选图片附件（base64 编码），供具备 vision 能力的模型使用。
    /// 通过 `metadata["images"]` 传给 daemon，可带多张。
    #[serde(default)]
    pub images: Vec<ImageInput>,
}

/// 单张图片输入（base64），字段对齐 `adaclaw_core::provider::ImageData`。
#[derive(Deserialize)]
pub struct ImageInput {
    /// MIME 类型，如 `"image/png"` / `"image/jpeg"`。
    pub media_type: String,
    /// base64 编码的图片字节（不含 `data:` 前缀）。
    pub data_base64: String,
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

    if req.message.trim().is_empty() && req.images.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "provide a non-empty 'message' and/or 'images'"})),
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

    // Images ride in metadata["images"] as a base64 array; the daemon turns them
    // into vision attachments (gated on the model's vision capability).
    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
    if !req.images.is_empty() {
        let arr: Vec<serde_json::Value> = req
            .images
            .iter()
            .map(|i| json!({"media_type": i.media_type, "data_base64": i.data_base64}))
            .collect();
        metadata.insert("images".to_string(), json!(arr));
    }

    // 构造 InboundMessage
    let msg = InboundMessage {
        id: Uuid::new_v4(),
        channel: "gateway".to_string(),
        session_id: session_id.clone(),
        sender_id: "gateway_api".to_string(),
        sender_name: "Gateway".to_string(),
        content: MessageContent::Text(req.message),
        reply_to: None,
        metadata,
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

// ── Streaming (Server-Sent Events) ───────────────────────────────────────────

/// One item pushed to an in-flight SSE response.
#[derive(Debug, Clone)]
pub enum StreamItem {
    /// Incremental assistant text.
    Delta(String),
    /// Terminal marker: the turn completed successfully.
    Done,
    /// Terminal marker: the turn failed (message is the error).
    Error(String),
}

/// Registry of in-flight streaming responses, keyed by a per-request stream id.
/// The daemon looks up the sender via [`push_stream`] to forward deltas; the
/// SSE handler owns the matching receiver.
#[allow(clippy::type_complexity)]
static STREAMS: OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<StreamItem>>>> =
    OnceLock::new();

fn streams() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<StreamItem>>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forward a [`StreamItem`] to the SSE response identified by `stream_id`.
///
/// Called by the daemon as the engine produces deltas. `Done`/`Error` are
/// terminal: the sender is removed so the receiver's stream ends and the SSE
/// connection closes. A no-op if the client already disconnected.
pub fn push_stream(stream_id: &str, item: StreamItem) {
    let mut map = match streams().lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    match item {
        StreamItem::Delta(_) => {
            if let Some(tx) = map.get(stream_id) {
                let _ = tx.send(item);
            }
        }
        // Terminal: send then drop the sender (removing it closes the stream).
        StreamItem::Done | StreamItem::Error(_) => {
            if let Some(tx) = map.remove(stream_id) {
                let _ = tx.send(item);
            }
        }
    }
}

/// `POST /v1/chat/stream`
///
/// Like [`chat`], but streams the assistant's reply token-by-token as
/// Server-Sent Events. Each `data:` line is a text delta; a final
/// `event: done` (`data: [DONE]`) closes the stream, or `event: error` on
/// failure. The request body is identical to [`ChatRequest`].
pub async fn chat_stream(body: Option<Json<ChatRequest>>) -> Response {
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

    if req.message.trim().is_empty() && req.images.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "provide a non-empty 'message' and/or 'images'"})),
        )
            .into_response();
    }

    let inbound_tx = match INBOUND_TX.get() {
        Some(tx) => tx,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Agent bus not connected — daemon not running"})),
            )
                .into_response();
        }
    };

    let session_id = req
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let stream_id = Uuid::new_v4().to_string();

    // Register the receiver before sending the message so no early delta is lost.
    let (tx, rx) = mpsc::unbounded_channel::<StreamItem>();
    streams().lock().unwrap().insert(stream_id.clone(), tx);

    // Metadata carries the stream id (so the daemon streams instead of replying
    // with one OutboundMessage) plus any images.
    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
    metadata.insert("stream_id".to_string(), json!(stream_id));
    if !req.images.is_empty() {
        let arr: Vec<serde_json::Value> = req
            .images
            .iter()
            .map(|i| json!({"media_type": i.media_type, "data_base64": i.data_base64}))
            .collect();
        metadata.insert("images".to_string(), json!(arr));
    }

    let msg = InboundMessage {
        id: Uuid::new_v4(),
        channel: "gateway".to_string(),
        session_id: session_id.clone(),
        sender_id: "gateway_api".to_string(),
        sender_name: "Gateway".to_string(),
        content: MessageContent::Text(req.message),
        reply_to: None,
        metadata,
    };

    if let Err(e) = inbound_tx.send(msg).await {
        streams().lock().unwrap().remove(&stream_id);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": format!("Failed to send to agent bus: {}", e)})),
        )
            .into_response();
    }

    // Map the receiver into SSE events. The stream ends when the sender is
    // dropped (on Done/Error or client disconnect).
    use tokio_stream::StreamExt;
    let events = UnboundedReceiverStream::new(rx).map(|item| {
        let ev = match item {
            StreamItem::Delta(t) => Event::default().data(t),
            StreamItem::Done => Event::default().event("done").data("[DONE]"),
            StreamItem::Error(e) => Event::default().event("error").data(e),
        };
        Ok::<Event, std::convert::Infallible>(ev)
    });

    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_images_optional() {
        let r: ChatRequest = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(r.message, "hi");
        assert!(r.images.is_empty());
    }

    #[test]
    fn chat_request_parses_multiple_images() {
        let body = r#"{
            "message": "compare these",
            "images": [
                {"media_type":"image/png","data_base64":"AAAA"},
                {"media_type":"image/jpeg","data_base64":"BBBB"}
            ]
        }"#;
        let r: ChatRequest = serde_json::from_str(body).unwrap();
        assert_eq!(r.images.len(), 2);
        assert_eq!(r.images[0].media_type, "image/png");
        assert_eq!(r.images[1].data_base64, "BBBB");
    }
}
