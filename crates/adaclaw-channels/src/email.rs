//! Email 渠道 — IMAP 收信 + SMTP 发信
//!
//! # 工作原理
//!
//! 1. 按配置间隔轮询 IMAP 服务器，获取未读邮件
//! 2. 解析发件人、主题、正文，通过 Bus 分发给 Agent
//! 3. `send()` 通过 SMTP 发送回复邮件
//!
//! # 安全门控
//!
//! 必须在配置中显式设置 `consent_granted = true` 才能启用，
//! 防止未经授权读取邮件。
//!
//! # 配置示例
//!
//! ```toml
//! [channels.email]
//! kind = "email"
//! allow_from = []          # 空 = 接受所有，非空 = 发件人白名单
//!
//! [channels.email.extra]
//! # ── 安全门控（必须设置为 true）────────────────────────────────────
//! consent_granted = "true"
//!
//! # ── IMAP 收信 ─────────────────────────────────────────────────────
//! imap_host = "imap.gmail.com"
//! imap_port = "993"
//! imap_username = "you@gmail.com"
//! imap_password = "app-password"    # Gmail App 密码
//!
//! # ── SMTP 发信 ─────────────────────────────────────────────────────
//! smtp_host = "smtp.gmail.com"
//! smtp_port = "587"                 # 587=STARTTLS, 465=TLS
//! smtp_username = "you@gmail.com"
//! smtp_password = "app-password"
//! from_address = "AdaClaw Agent <you@gmail.com>"
//!
//! # ── 可选配置 ──────────────────────────────────────────────────────
//! auto_reply_enabled = "true"       # false = 只读取，不发送回复
//! poll_interval_secs = "60"         # 轮询间隔（秒）
//! ```

use crate::base::BaseChannel;
use adaclaw_core::channel::{Channel, MessageBus, MessageContent, OutboundMessage};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

// ── EmailChannel ──────────────────────────────────────────────────────────────

/// Email 渠道（IMAP 收信 + SMTP 发信）
///
/// # 配置说明
///
/// 所有配置从 `ChannelConfig.extra` 中读取（string map）：
/// - `consent_granted` = "true"（必须显式设置）
/// - `imap_host` / `imap_port` / `imap_username` / `imap_password`
/// - `smtp_host` / `smtp_port` / `smtp_username` / `smtp_password`
/// - `from_address`（发件人地址/显示名）
/// - `auto_reply_enabled`（默认 "true"）
/// - `poll_interval_secs`（默认 "60"）
pub struct EmailChannel {
    base: Arc<BaseChannel>,
    /// IMAP 配置
    imap_host: String,
    imap_port: u16,
    imap_username: String,
    imap_password: String,
    /// SMTP 配置
    smtp_host: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
    from_address: String,
    /// 是否自动发送回复（false = 只读取）
    auto_reply_enabled: bool,
    /// 轮询间隔（秒）
    poll_interval_secs: u64,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl EmailChannel {
    /// 从 `ChannelConfig.extra` 构建 EmailChannel。
    ///
    /// # 安全
    /// 必须显式设置 `extra.consent_granted = "true"`，否则返回错误。
    pub fn from_extra(allow_from: Vec<String>, extra: &HashMap<String, String>) -> Result<Self> {
        // ── 安全门控 ──────────────────────────────────────────────────
        let consent = extra
            .get("consent_granted")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        if !consent {
            return Err(anyhow!(
                "Email channel requires explicit consent: set extra.consent_granted = \"true\" \
                 in your config to acknowledge that AdaClaw will read your emails."
            ));
        }

        // ── IMAP ──────────────────────────────────────────────────────
        let imap_host = extra
            .get("imap_host")
            .cloned()
            .ok_or_else(|| anyhow!("channels.email.extra.imap_host is required"))?;
        let imap_port = extra
            .get("imap_port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(993u16);
        let imap_username = extra
            .get("imap_username")
            .cloned()
            .ok_or_else(|| anyhow!("channels.email.extra.imap_username is required"))?;
        let imap_password = extra
            .get("imap_password")
            .cloned()
            .ok_or_else(|| anyhow!("channels.email.extra.imap_password is required"))?;

        // ── SMTP ──────────────────────────────────────────────────────
        let smtp_host = extra
            .get("smtp_host")
            .cloned()
            .ok_or_else(|| anyhow!("channels.email.extra.smtp_host is required"))?;
        let smtp_port = extra
            .get("smtp_port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(587u16);
        let smtp_username = extra
            .get("smtp_username")
            .cloned()
            .unwrap_or_else(|| imap_username.clone());
        let smtp_password = extra
            .get("smtp_password")
            .cloned()
            .unwrap_or_else(|| imap_password.clone());
        let from_address = extra
            .get("from_address")
            .cloned()
            .unwrap_or_else(|| smtp_username.clone());

        // ── 可选 ──────────────────────────────────────────────────────
        let auto_reply_enabled = extra
            .get("auto_reply_enabled")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true);
        let poll_interval_secs = extra
            .get("poll_interval_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(60u64);

        let base = Arc::new(BaseChannel::new("email").with_allow_from(allow_from));

        Ok(Self {
            base,
            imap_host,
            imap_port,
            imap_username,
            imap_password,
            smtp_host,
            smtp_port,
            smtp_username,
            smtp_password,
            from_address,
            auto_reply_enabled,
            poll_interval_secs,
            shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// 在 spawn_blocking 内执行 IMAP 轮询（同步 imap 2.x API）。
    ///
    /// 使用 `imap` crate v2（stable）在 blocking 线程池上运行，避免阻塞 tokio 运行时。
    fn poll_inbox_blocking(
        imap_host: &str,
        imap_port: u16,
        username: &str,
        password: &str,
    ) -> Result<Vec<EmailMessage>> {
        // 构建 TLS 连接器
        let tls = native_tls::TlsConnector::builder()
            .build()
            .map_err(|e| anyhow!("TLS connector build failed: {}", e))?;

        // 连接 IMAP 服务器（imap 2.x API）
        let client = imap::connect((imap_host, imap_port), imap_host, &tls)
            .map_err(|e| anyhow!("IMAP connect failed: {}", e))?;

        let mut session = client
            .login(username, password)
            .map_err(|(e, _)| anyhow!("IMAP login failed: {}", e))?;

        // 选择 INBOX
        session
            .select("INBOX")
            .map_err(|e| anyhow!("IMAP SELECT INBOX failed: {}", e))?;

        // 搜索未读消息
        let unseen = session
            .search("UNSEEN")
            .map_err(|e| anyhow!("IMAP SEARCH UNSEEN failed: {}", e))?;

        if unseen.is_empty() {
            let _ = session.logout();
            return Ok(vec![]);
        }

        let seq_set: Vec<String> = unseen.iter().map(|n| n.to_string()).collect();
        let seq_str = seq_set.join(",");

        // 获取完整邮件（RFC822）
        let messages = session
            .fetch(&seq_str, "RFC822")
            .map_err(|e| anyhow!("IMAP FETCH failed: {}", e))?;

        let mut result = Vec::new();
        for msg in messages.iter() {
            if let Some(body) = msg.body() {
                match parse_email_message(body) {
                    Ok(parsed) => result.push(parsed),
                    Err(e) => warn!(channel = "email", error = %e, "Failed to parse email"),
                }
            }
        }

        // 标记为已读
        if !seq_set.is_empty() {
            let _ = session.store(&seq_str, "+FLAGS (\\Seen)");
        }

        let _ = session.logout();
        Ok(result)
    }
}

// ── 邮件解析 ──────────────────────────────────────────────────────────────────

/// 已解析的邮件消息
#[derive(Debug, Clone)]
pub struct EmailMessage {
    /// 发件人地址（如 `user@example.com`）
    pub from_addr: String,
    /// 发件人显示名（可为空）
    pub from_name: String,
    /// 邮件主题
    pub subject: String,
    /// 纯文本正文
    pub body: String,
    /// Message-ID（用于 In-Reply-To）
    pub message_id: String,
}

/// 从 RFC822 原始字节解析邮件
fn parse_email_message(raw: &[u8]) -> Result<EmailMessage> {
    use mailparse::MailHeaderMap;

    let parsed = mailparse::parse_mail(raw).map_err(|e| anyhow!("mailparse error: {}", e))?;

    // 提取 From 头
    let from_header = parsed.headers.get_first_value("From").unwrap_or_default();
    let (from_name, from_addr) = parse_from_header(&from_header);

    // 提取 Subject
    let subject = parsed
        .headers
        .get_first_value("Subject")
        .unwrap_or_default();

    // 提取 Message-ID
    let message_id = parsed
        .headers
        .get_first_value("Message-ID")
        .unwrap_or_default();

    // 提取纯文本正文
    let body = extract_text_body(&parsed)?;

    Ok(EmailMessage {
        from_addr,
        from_name,
        subject,
        body,
        message_id,
    })
}

/// 解析 "Name <addr>" 格式的 From 头
fn parse_from_header(header: &str) -> (String, String) {
    let header = header.trim();
    if let Some(start) = header.find('<')
        && let Some(end) = header.find('>')
    {
        let name = header[..start].trim().trim_matches('"').to_string();
        let addr = header[start + 1..end].trim().to_string();
        return (name, addr);
    }
    // 没有 <> 格式，整个就是地址
    (String::new(), header.to_string())
}

/// 从 MIME 消息中提取 text/plain 正文
fn extract_text_body(mail: &mailparse::ParsedMail<'_>) -> Result<String> {
    // 单部分消息
    if mail.subparts.is_empty() {
        let ctype = mail.ctype.mimetype.to_lowercase();
        if ctype.starts_with("text/plain") || ctype.starts_with("text/") {
            return mail
                .get_body()
                .map_err(|e| anyhow!("mailparse get_body error: {}", e));
        }
        return Ok(String::new());
    }

    // 多部分消息，优先 text/plain
    for part in &mail.subparts {
        let ctype = part.ctype.mimetype.to_lowercase();
        if ctype.starts_with("text/plain") {
            return part
                .get_body()
                .map_err(|e| anyhow!("mailparse get_body error: {}", e));
        }
    }

    // 回退到 text/html（剥离标签）
    for part in &mail.subparts {
        let ctype = part.ctype.mimetype.to_lowercase();
        if ctype.starts_with("text/html") {
            let html = part
                .get_body()
                .map_err(|e| anyhow!("mailparse get_body error: {}", e))?;
            // 简单剥离 HTML 标签
            let text = html
                .replace("<br>", "\n")
                .replace("<br/>", "\n")
                .replace("<br />", "\n")
                .replace("<p>", "\n")
                .replace("</p>", "\n");
            let stripped: String = text
                .chars()
                .scan(false, |in_tag, c| {
                    if c == '<' {
                        *in_tag = true;
                        Some(None)
                    } else if c == '>' {
                        *in_tag = false;
                        Some(None)
                    } else if *in_tag {
                        Some(None)
                    } else {
                        Some(Some(c))
                    }
                })
                .flatten()
                .collect();
            return Ok(stripped.trim().to_string());
        }
    }

    // 递归处理嵌套 multipart
    for part in &mail.subparts {
        let body = extract_text_body(part)?;
        if !body.is_empty() {
            return Ok(body);
        }
    }

    Ok(String::new())
}

// ── SMTP 发信 ─────────────────────────────────────────────────────────────────

/// 在 spawn_blocking 内通过 SMTP 发送邮件（同步 lettre）
#[allow(clippy::too_many_arguments)]
fn send_email_blocking(
    smtp_host: &str,
    smtp_port: u16,
    username: &str,
    password: &str,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    let email = Message::builder()
        .from(
            from.parse()
                .map_err(|_| anyhow!("Invalid from address: {}", from))?,
        )
        .to(to
            .parse()
            .map_err(|_| anyhow!("Invalid to address: {}", to))?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| anyhow!("Failed to build email: {}", e))?;

    let creds = Credentials::new(username.to_string(), password.to_string());

    // 根据端口选择传输方式
    let mailer = if smtp_port == 465 {
        // TLS（SSL over SMTP）
        SmtpTransport::relay(smtp_host)
            .map_err(|e| anyhow!("SMTP relay error: {}", e))?
            .credentials(creds)
            .port(smtp_port)
            .build()
    } else {
        // STARTTLS（587 或 25）
        SmtpTransport::starttls_relay(smtp_host)
            .map_err(|e| anyhow!("SMTP STARTTLS relay error: {}", e))?
            .credentials(creds)
            .port(smtp_port)
            .build()
    };

    mailer
        .send(&email)
        .map_err(|e| anyhow!("SMTP send failed: {}", e))?;

    Ok(())
}

// ── Channel trait 实现 ────────────────────────────────────────────────────────

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &str {
        "email"
    }

    fn is_running(&self) -> bool {
        self.base.is_running()
    }

    async fn start(&self, bus: Arc<dyn MessageBus>) -> Result<()> {
        self.base.set_running(true);
        info!(
            channel = "email",
            imap_host = %self.imap_host,
            poll_interval_secs = %self.poll_interval_secs,
            "Email channel started (IMAP polling)"
        );

        let (tx, mut rx) = oneshot::channel::<()>();
        *self.shutdown_tx.lock().await = Some(tx);

        let mut interval = tokio::time::interval(Duration::from_secs(self.poll_interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 克隆所需字段（供 spawn_blocking 使用）
        let imap_host = self.imap_host.clone();
        let imap_port = self.imap_port;
        let imap_username = self.imap_username.clone();
        let imap_password = self.imap_password.clone();
        let base = Arc::clone(&self.base);
        let bus = Arc::clone(&bus);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // 在 blocking 线程池中执行同步 IMAP
                    let host = imap_host.clone();
                    let uname = imap_username.clone();
                    let pass = imap_password.clone();

                    let messages = tokio::task::spawn_blocking(move || {
                        Self::poll_inbox_blocking(&host, imap_port, &uname, &pass)
                    })
                    .await
                    .unwrap_or_else(|e| Err(anyhow!("spawn_blocking error: {}", e)));

                    match messages {
                        Ok(msgs) => {
                            for msg in msgs {
                                debug!(
                                    channel = "email",
                                    from = %msg.from_addr,
                                    subject = %msg.subject,
                                    "New email received"
                                );

                                // 白名单检查
                                if !base.is_allowed(&msg.from_addr) {
                                    warn!(
                                        channel = "email",
                                        from = %msg.from_addr,
                                        "Email from non-whitelisted sender, ignoring"
                                    );
                                    continue;
                                }

                                // 构建消息内容：[Subject: ...]\n\n{body}
                                let content = if msg.subject.is_empty() {
                                    msg.body.clone()
                                } else {
                                    format!("[Subject: {}]\n\n{}", msg.subject, msg.body)
                                };

                                let sender_name = if msg.from_name.is_empty() {
                                    msg.from_addr.clone()
                                } else {
                                    format!("{} <{}>", msg.from_name, msg.from_addr)
                                };

                                let mut metadata = std::collections::HashMap::new();
                                metadata.insert(
                                    "message_id".to_string(),
                                    serde_json::Value::String(msg.message_id.clone()),
                                );
                                metadata.insert(
                                    "subject".to_string(),
                                    serde_json::Value::String(msg.subject.clone()),
                                );

                                base.handle_message(
                                    &bus,
                                    &msg.from_addr,
                                    &sender_name,
                                    &msg.from_addr, // session_id = from address
                                    content.trim(),
                                    metadata,
                                )
                                .await;
                            }
                        }
                        Err(e) => {
                            error!(
                                channel = "email",
                                error = %e,
                                "IMAP polling error, will retry next interval"
                            );
                        }
                    }
                }

                _ = &mut rx => {
                    info!(channel = "email", "Email channel stop signal received");
                    break;
                }
            }
        }

        self.base.set_running(false);
        info!(channel = "email", "Email channel stopped");
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if !self.auto_reply_enabled {
            debug!(
                channel = "email",
                "auto_reply_enabled = false, skipping send"
            );
            return Ok(());
        }

        let to = msg.target_session_id.clone();
        let content = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => return Ok(()),
        };

        let smtp_host = self.smtp_host.clone();
        let smtp_port = self.smtp_port;
        let smtp_username = self.smtp_username.clone();
        let smtp_password = self.smtp_password.clone();
        let from_address = self.from_address.clone();

        // 在 blocking 线程池中发送同步 SMTP
        tokio::task::spawn_blocking(move || {
            send_email_blocking(
                &smtp_host,
                smtp_port,
                &smtp_username,
                &smtp_password,
                &from_address,
                &to,
                "Re: AdaClaw Reply",
                &content,
            )
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking error: {}", e))??;

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
