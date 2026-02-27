//! 企业微信（WeCom / WeChat Work）AIBot 渠道
//!
//! # 工作原理（AIBot 智能机器人模式）
//!
//! 1. 企业微信将消息以 HTTP POST（加密 XML）发到本渠道 Webhook 地址
//! 2. 本渠道验证 SHA1 签名，AES-CBC 解密消息体（注意：block_size=32，非标准！）
//! 3. 解析 JSON 消息，发布到 MessageBus
//! 4. Agent 完成后，`send()` 方法 POST 回复到消息中携带的 `response_url`
//!
//! # 关键细节（参考 picoclaw wecom.go）
//!
//! - AES key = base64decode(EncodingAESKey + "=")（43 字节 base64 → 32 字节）
//! - IV = AES key 前 16 字节
//! - PKCS7 padding block_size = **32**（非 AES 标准 16！）
//! - 明文格式：random(16) + msg_len(4, big-endian) + msg_json + aibotid
//!
//! # 配置示例
//!
//! ```toml
//! [channels.wecom]
//! kind = "wechat_work"
//! token = "your_token"             # 企业微信后台 Token（签名验证）
//! allow_from = []
//!
//! [channels.wecom.extra]
//! webhook_port = "9003"
//! webhook_path = "/webhook/wecom"
//! encoding_aes_key = "..."         # 43 字符 Base64 密钥
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use axum::{
    extract::{Query, State},
    http::{StatusCode},
    routing::{get, post},
    Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

// ── 企业微信消息结构（AIBot JSON 格式）──────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct WeComMessage {
    #[serde(default)]
    msgid: String,
    #[serde(default)]
    msgtype: String,
    #[serde(default)]
    from: WeComFrom,
    #[serde(default)]
    chatid: String,
    #[serde(default)]
    chattype: String,
    #[serde(default)]
    response_url: String,
    text: Option<WeComText>,
    voice: Option<WeComVoice>,
    image: Option<WeComMedia>,
    file: Option<WeComMedia>,
    mixed: Option<WeComMixed>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct WeComFrom {
    #[serde(default)]
    userid: String,
}

#[derive(Debug, Deserialize, Clone)]
struct WeComText {
    content: String,
}

#[derive(Debug, Deserialize, Clone)]
struct WeComVoice {
    #[serde(default)]
    content: String, // 语音转文字结果
}

#[derive(Debug, Deserialize, Clone)]
struct WeComMedia {
    #[serde(default)]
    url: String,
}

#[derive(Debug, Deserialize, Clone)]
struct WeComMixed {
    #[serde(default)]
    msg_item: Vec<WeComMixedItem>,
}

#[derive(Debug, Deserialize, Clone)]
struct WeComMixedItem {
    #[serde(default)]
    msgtype: String,
    text: Option<WeComText>,
}

#[derive(Debug, Serialize)]
struct WeComReply {
    msgtype: String,
    text: WeComReplyText,
}

#[derive(Debug, Serialize)]
struct WeComReplyText {
    content: String,
}

// ── Webhook 查询参数 ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WeComWebhookQuery {
    #[serde(default)]
    msg_signature: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    echostr: String,
}

// ── 加密 XML 结构 ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename = "xml")]
struct WeComEncryptXml {
    #[serde(rename = "Encrypt")]
    encrypt: String,
}

// ── 共享状态 ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct WeComState {
    base: Arc<BaseChannel>,
    token: String,
    encoding_aes_key: Option<String>,
    bus: Arc<dyn MessageBus>,
    http_client: reqwest::Client,
    /// 消息去重：msg_id → bool（ring buffer 简化版，使用 HashMap + 容量限制）
    processed_msgs: Arc<tokio::sync::Mutex<HashMap<String, ()>>>,
}

impl WeComState {
    /// SHA1 签名验证
    /// sort([token, timestamp, nonce, msg_encrypt]) → SHA1 → hex
    fn verify_signature(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        msg_encrypt: &str,
    ) -> bool {
        if self.token.is_empty() {
            return true; // 未配置 token，跳过验证
        }
        let mut params = vec![
            self.token.as_str(),
            timestamp,
            nonce,
            msg_encrypt,
        ];
        params.sort_unstable();
        let joined = params.join("");
        let mut hasher = Sha1::new();
        hasher.update(joined.as_bytes());
        let result = format!("{:x}", hasher.finalize());
        result == msg_signature
    }

    /// AES-CBC 解密企业微信消息
    ///
    /// 明文格式：random(16) + msg_len(4, big-endian) + msg + aibotid
    /// PKCS7 padding block_size = 32（企业微信自定义，非 AES 标准 16）
    fn decrypt_message(&self, encrypted_msg: &str) -> Result<String> {
        use aes::Aes256;
        use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
        type CbcAes256Dec = cbc::Decryptor<Aes256>;

        let aes_key_b64 = match &self.encoding_aes_key {
            Some(k) => format!("{}=", k),
            None => return Err(anyhow!("encoding_aes_key not configured")),
        };

        let aes_key = base64::engine::general_purpose::STANDARD
            .decode(&aes_key_b64)
            .map_err(|e| anyhow!("Failed to decode AES key: {}", e))?;

        if aes_key.len() != 32 {
            return Err(anyhow!(
                "AES key must be 32 bytes, got {}",
                aes_key.len()
            ));
        }

        let iv = &aes_key[..16];
        let cipher_bytes = base64::engine::general_purpose::STANDARD
            .decode(encrypted_msg)
            .map_err(|e| anyhow!("Failed to decode encrypted message: {}", e))?;

        let mut buf = cipher_bytes.clone();
        let decrypted = CbcAes256Dec::new_from_slices(&aes_key, iv)
            .map_err(|e| anyhow!("Failed to create AES decryptor: {:?}", e))?
            .decrypt_padded_mut::<NoPadding>(&mut buf)
            .map_err(|e| anyhow!("AES decryption failed: {:?}", e))?;

        // 移除 WeCom 自定义 PKCS7 padding（block_size = 32）
        if decrypted.is_empty() {
            return Err(anyhow!("Decrypted data is empty"));
        }
        let pad = decrypted[decrypted.len() - 1] as usize;
        if pad == 0 || pad > 32 {
            return Err(anyhow!("Invalid PKCS7 padding: {}", pad));
        }
        let data = &decrypted[..decrypted.len() - pad];

        // 解析明文结构：random(16) + msg_len(4) + msg + aibotid
        if data.len() < 20 {
            return Err(anyhow!("Decrypted data too short: {} bytes", data.len()));
        }
        let msg_len =
            u32::from_be_bytes([data[16], data[17], data[18], data[19]]) as usize;
        if data.len() < 20 + msg_len {
            return Err(anyhow!(
                "Invalid message length: {} > available {}",
                msg_len,
                data.len() - 20
            ));
        }
        let msg =
            String::from_utf8(data[20..20 + msg_len].to_vec())
                .map_err(|e| anyhow!("Invalid UTF-8 in decrypted message: {}", e))?;

        Ok(msg)
    }

    /// 消息去重（防止重复处理）
    async fn is_duplicate(&self, msg_id: &str) -> bool {
        let mut map = self.processed_msgs.lock().await;
        if map.contains_key(msg_id) {
            return true;
        }
        // 超过 1000 条时清空（简单策略）
        if map.len() > 1000 {
            map.clear();
        }
        map.insert(msg_id.to_string(), ());
        false
    }
}

// ── WeComChannel ──────────────────────────────────────────────────────────────

pub struct WeComChannel {
    base: Arc<BaseChannel>,
    token: String,
    encoding_aes_key: Option<String>,
    webhook_port: u16,
    webhook_path: String,
    http_client: reqwest::Client,
    processed_msgs: Arc<tokio::sync::Mutex<HashMap<String, ()>>>,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl WeComChannel {
    pub fn new(
        token: String,
        encoding_aes_key: Option<String>,
        allow_from: Vec<String>,
        webhook_port: u16,
        webhook_path: String,
    ) -> Self {
        let base = Arc::new(BaseChannel::new("wechat_work").with_allow_from(allow_from));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base,
            token,
            encoding_aes_key,
            webhook_port,
            webhook_path,
            http_client: client,
            processed_msgs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Channel for WeComChannel {
    fn name(&self) -> &str {
        "wechat_work"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.webhook_port)
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("Invalid webhook port {}: {}", self.webhook_port, e))?;

        let state = WeComState {
            base: Arc::clone(&self.base),
            token: self.token.clone(),
            encoding_aes_key: self.encoding_aes_key.clone(),
            bus,
            http_client: self.http_client.clone(),
            processed_msgs: Arc::clone(&self.processed_msgs),
        };

        let path = self.webhook_path.clone();
        let app = Router::new()
            .route(&path, get(handle_wecom_verify).post(handle_wecom_message))
            .route("/health/wecom", get(|| async { StatusCode::OK }))
            .with_state(state);

        let (tx, rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        self.base.set_running(true);
        info!(
            channel = "wechat_work",
            addr = %addr,
            path = %self.webhook_path,
            "WeCom webhook server started"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async { rx.await.ok(); })
            .await
            .map_err(|e| anyhow!("WeCom HTTP server error: {}", e))?;

        self.base.set_running(false);
        info!(channel = "wechat_work", "WeCom channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let response_url = &msg.target_session_id;
        if response_url.is_empty() || !response_url.starts_with("http") {
            return Err(anyhow!(
                "WeCom: invalid session_id (expected response_url): '{}'",
                response_url
            ));
        }

        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        let reply = WeComReply {
            msgtype: "text".to_string(),
            text: WeComReplyText { content },
        };

        let resp = self
            .http_client
            .post(response_url)
            .json(&reply)
            .send()
            .await
            .map_err(|e| anyhow!("WeCom reply POST failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "WeCom reply failed: HTTP {} — {}",
                status,
                body
            ));
        }

        debug!(channel = "wechat_work", "Reply sent successfully");
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

// ── Axum Handlers ─────────────────────────────────────────────────────────────

/// GET: 企业微信 URL 验证请求
async fn handle_wecom_verify(
    State(state): State<WeComState>,
    Query(params): Query<WeComWebhookQuery>,
) -> (StatusCode, String) {
    if !state.verify_signature(
        &params.msg_signature,
        &params.timestamp,
        &params.nonce,
        &params.echostr,
    ) {
        warn!(channel = "wechat_work", "Signature verification failed on URL verify");
        return (StatusCode::FORBIDDEN, String::new());
    }

    // 解密 echostr
    match state.decrypt_message(&params.echostr) {
        Ok(plain) => {
            // 按企业微信文档：去掉 BOM 和前后空白
            let plain = plain
                .trim()
                .trim_start_matches('\u{FEFF}')
                .to_string();
            (StatusCode::OK, plain)
        }
        Err(e) => {
            error!(channel = "wechat_work", error = %e, "Failed to decrypt echostr");
            (StatusCode::INTERNAL_SERVER_ERROR, String::new())
        }
    }
}

/// POST: 企业微信消息回调
async fn handle_wecom_message(
    State(state): State<WeComState>,
    Query(params): Query<WeComWebhookQuery>,
    body: String,
) -> StatusCode {
    // 解析加密 XML
    let encrypted: WeComEncryptXml = match quick_xml_parse(&body) {
        Ok(x) => x,
        Err(e) => {
            error!(channel = "wechat_work", error = %e, "Failed to parse XML");
            return StatusCode::BAD_REQUEST;
        }
    };

    // 签名验证
    if !state.verify_signature(
        &params.msg_signature,
        &params.timestamp,
        &params.nonce,
        &encrypted.encrypt,
    ) {
        warn!(channel = "wechat_work", "Signature verification failed on message");
        return StatusCode::FORBIDDEN;
    }

    // 解密消息体
    let msg_json = match state.decrypt_message(&encrypted.encrypt) {
        Ok(j) => j,
        Err(e) => {
            error!(channel = "wechat_work", error = %e, "Decryption failed");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    // 解析 JSON 消息
    let msg: WeComMessage = match serde_json::from_str(&msg_json) {
        Ok(m) => m,
        Err(e) => {
            error!(
                channel = "wechat_work",
                error = %e,
                raw = %msg_json,
                "Failed to parse WeCom message JSON"
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    // 异步处理消息（立即返回 success，避免超时）
    tokio::spawn(async move {
        process_wecom_message(state, msg).await;
    });

    // 企业微信要求立即返回 "success"
    StatusCode::OK
}

async fn process_wecom_message(state: WeComState, msg: WeComMessage) {
    // 消息去重
    if !msg.msgid.is_empty() && state.is_duplicate(&msg.msgid).await {
        debug!(channel = "wechat_work", msg_id = %msg.msgid, "Duplicate message, skipping");
        return;
    }

    // 提取文本内容
    let content = match msg.msgtype.as_str() {
        "text" => msg.text.as_ref().map(|t| t.content.clone()),
        "voice" => msg.voice.as_ref().map(|v| v.content.clone()),
        "mixed" => msg.mixed.as_ref().map(|m| {
            m.msg_item
                .iter()
                .filter(|i| i.msgtype == "text")
                .filter_map(|i| i.text.as_ref().map(|t| t.content.clone()))
                .collect::<Vec<_>>()
                .join(" ")
        }),
        _ => {
            debug!(
                channel = "wechat_work",
                msg_type = %msg.msgtype,
                "Unsupported message type, skipping"
            );
            return;
        }
    };

    let content = match content {
        Some(c) if !c.is_empty() => c,
        _ => {
            debug!(channel = "wechat_work", "Empty message content, skipping");
            return;
        }
    };

    let sender_id = msg.from.userid.clone();
    // session_id = response_url（send() 用此 URL 回复）
    let session_id = msg.response_url.clone();

    let mut metadata: HashMap<String, Value> = HashMap::new();
    metadata.insert("msg_id".to_string(), Value::String(msg.msgid.clone()));
    metadata.insert("msg_type".to_string(), Value::String(msg.msgtype.clone()));
    metadata.insert("chat_type".to_string(), Value::String(msg.chattype.clone()));
    metadata.insert("response_url".to_string(), Value::String(msg.response_url.clone()));
    if !msg.chatid.is_empty() {
        metadata.insert("chat_id".to_string(), Value::String(msg.chatid.clone()));
    }

    debug!(
        channel = "wechat_work",
        sender_id = %sender_id,
        preview = %content.chars().take(60).collect::<String>(),
        "WeCom message received"
    );

    state
        .base
        .handle_message(&state.bus, &sender_id, &sender_id, &session_id, &content, metadata)
        .await;
}

// ── 简易 XML 解析（避免引入 quick-xml 依赖，用手工解析）────────────────────────

fn quick_xml_parse(xml: &str) -> Result<WeComEncryptXml> {
    // <xml><Encrypt>...</Encrypt>...</xml>
    let encrypt = extract_xml_tag(xml, "Encrypt")
        .ok_or_else(|| anyhow!("Missing <Encrypt> tag in XML"))?;
    Ok(WeComEncryptXml { encrypt })
}

fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}
