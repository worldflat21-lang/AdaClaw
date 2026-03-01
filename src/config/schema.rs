use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Top-level Config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Schema version number.  Absent in old configs (treated as 0).
    /// Current version: 1.  Bump whenever a breaking change is made.
    #[serde(default)]
    pub config_version: u32,

    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub channels: HashMap<String, ChannelConfig>,

    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    #[serde(default)]
    pub routing: Vec<RoutingRule>,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub gateway: GatewayConfig,

    /// Observability backend configuration (Phase 7).
    #[serde(default)]
    pub observability: ObservabilityConfig,

    /// Tunnel configuration (Phase 7).
    #[serde(default)]
    pub tunnel: TunnelConfig,

    /// Tools / MCP server configuration (Phase 10).
    #[serde(default)]
    pub tools: ToolsConfig,

    /// Heartbeat / active task configuration (Phase 10).
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
}

impl Config {
    /// Load config from a TOML file, apply version migration, then apply
    /// environment variable overrides.
    ///
    /// Returns `Err` if the file cannot be read, the TOML is malformed, or the
    /// stored `config_version` is newer than this binary supports.
    ///
    /// Migration notes (e.g. "added config_version = 1") are emitted as
    /// `tracing::warn!` messages so they surface in daemon logs without
    /// interrupting the startup sequence.
    pub fn load_from_file(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config file '{}': {}", path, e))?;

        let raw: Config = toml::from_str(&text).map_err(|e| {
            // Enrich the toml error with the file path so users can find it fast
            anyhow::anyhow!("Config file '{}' has a TOML syntax error:\n  {}", path, e)
        })?;

        // Run forward migrations.  On error (future version) we propagate
        // immediately — there is nothing safe we can do.
        let (mut cfg, notes) = super::migration::migrate(raw)?;
        for note in &notes {
            tracing::warn!("[config migration] {}", note);
        }

        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// Load config from `config.toml` in the current directory (best-effort).
    ///
    /// Falls back to a default config on any error (missing file, parse
    /// failures, etc.).  For production use prefer [`load_from_file`] so
    /// errors are visible.
    pub fn load() -> Self {
        Self::load_from_file("config.toml").unwrap_or_default().with_env()
    }

    /// Run semantic validation and return all field-level errors found.
    ///
    /// An empty `Vec` means the config is valid.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let cfg = Config::load_from_file("config.toml")?;
    /// let errors = cfg.validate();
    /// if !errors.is_empty() {
    ///     for e in &errors { eprintln!("  • {e}"); }
    ///     anyhow::bail!("{} config error(s) found.", errors.len());
    /// }
    /// ```
    pub fn validate(&self) -> Vec<super::validation::ValidationError> {
        super::validation::validate(self)
    }

    fn with_env(mut self) -> Self {
        self.apply_env_overrides();
        self
    }

    /// Apply environment variable overrides on top of file config.
    /// Priority: env vars > config file.
    fn apply_env_overrides(&mut self) {
        // ADACLAW_OPENAI_API_KEY → providers["openai"].api_key
        if let Ok(key) = std::env::var("ADACLAW_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
        {
            self.providers
                .entry("openai".to_string())
                .or_default()
                .api_key = Some(key);
        }

        // ADACLAW_ANTHROPIC_API_KEY
        if let Ok(key) = std::env::var("ADACLAW_ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        {
            self.providers
                .entry("anthropic".to_string())
                .or_default()
                .api_key = Some(key);
        }

        // ADACLAW_OLLAMA_URL
        if let Ok(url) = std::env::var("ADACLAW_OLLAMA_URL") {
            self.providers
                .entry("ollama".to_string())
                .or_default()
                .base_url = Some(url);
        }

        // ADACLAW_TELEGRAM_TOKEN
        if let Ok(token) = std::env::var("ADACLAW_TELEGRAM_TOKEN") {
            self.channels
                .entry("telegram".to_string())
                .or_insert_with(|| ChannelConfig {
                    kind: "telegram".to_string(),
                    ..Default::default()
                })
                .token = Some(token);
        }

        // ADACLAW_BEARER_TOKEN (gateway auth)
        if let Ok(tok) = std::env::var("ADACLAW_BEARER_TOKEN") {
            self.gateway.bearer_token = Some(tok);
        }

        // ADACLAW_WORKSPACE
        if let Ok(ws) = std::env::var("ADACLAW_WORKSPACE") {
            self.security.workspace = Some(ws);
        }

        // ADACLAW_AUTONOMY_LEVEL
        if let Ok(level) = std::env::var("ADACLAW_AUTONOMY_LEVEL") {
            self.security.autonomy_level = level;
        }
    }
}

// ── Provider ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    /// API key (also overrideable via ADACLAW_<PROVIDER>_API_KEY env var)
    pub api_key: Option<String>,
    /// Override the base URL (for OpenAI-compatible endpoints, self-hosted, proxies)
    pub base_url: Option<String>,
    /// Default model to use for this provider
    pub default_model: Option<String>,
    /// Request timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

// ── Memory ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Backend: "sqlite" | "markdown" | "none"
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Path to the SQLite database file or Markdown directory (default: "memory.db")
    #[serde(default = "default_memory_path")]
    pub path: String,
    /// Embedding provider for semantic search: "fastembed" | "openai" | "none"
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// API key for OpenAI embedding (can also use OPENAI_API_KEY env var)
    pub embed_api_key: Option<String>,
    /// Base URL override for embedding API (e.g. self-hosted proxy)
    pub embed_base_url: Option<String>,
    /// RRF vector weight (0.0–1.0, unused when embedding_provider = "none")
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f32,
    /// RRF keyword weight (0.0–1.0)
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f32,
    /// TTL in days per category — 0 means never expire.
    /// Example: { Core = 0, Daily = 30, Conversation = 7 }
    #[serde(default)]
    pub ttl_days: HashMap<String, u32>,
}

fn default_memory_backend() -> String {
    "sqlite".to_string()
}
fn default_memory_path() -> String {
    "memory.db".to_string()
}
fn default_embedding_provider() -> String {
    "none".to_string()
}
fn default_vector_weight() -> f32 {
    0.5
}
fn default_keyword_weight() -> f32 {
    0.5
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: default_memory_backend(),
            path: default_memory_path(),
            embedding_provider: default_embedding_provider(),
            embed_api_key: None,
            embed_base_url: None,
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            ttl_days: HashMap::new(),
        }
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    /// Channel type: "telegram" | "cli" | "discord" | "slack" | "dingtalk" | "feishu" | "wechat_work" | "webhook"
    pub kind: String,
    /// Bot/app token (Telegram, Discord, Slack…)
    pub token: Option<String>,
    /// Webhook secret / signing secret for HMAC verification
    pub webhook_secret: Option<String>,
    /// Polling vs Webhook mode (default: polling)
    #[serde(default)]
    pub use_webhook: bool,
    /// Webhook URL (required when use_webhook = true, or used as reply URL for DingTalk/WeCom)
    pub webhook_url: Option<String>,

    // ── Phase 6 per-channel access control ────────────────────────────────────

    /// Per-channel sender allowlist (deny-by-default when non-empty).
    /// Supports "id|username" compound format for Telegram etc.
    /// Takes priority over the global SecurityConfig.allowlist.
    #[serde(default)]
    pub allow_from: Vec<String>,

    /// Separate allowlist for group chats (Discord guilds, Slack channels, etc.).
    /// Falls back to allow_from when empty.
    #[serde(default)]
    pub allow_from_groups: Vec<String>,

    /// Only respond in group chats when the bot is @mentioned (default: false).
    #[serde(default)]
    pub require_mention: bool,

    /// Send tool-call progress messages to this channel (default: true).
    #[serde(default = "default_true")]
    pub send_progress: bool,

    /// Additional channel-specific key-value settings.
    ///
    /// Common keys:
    /// - `webhook_port`: HTTP port for webhook channels (DingTalk, Feishu, WeCom, Slack, generic)
    /// - `webhook_path`: URL path override (default varies per channel)
    /// - `app_id` / `app_secret`: Feishu / WeCom App credentials
    /// - `encoding_aes_key`: WeCom AES message decryption key
    /// - `verification_token`: Feishu verification token
    /// - `bot_token` / `signing_secret`: Slack extra tokens
    /// - `intents`: Discord Gateway intent bitmask (string)
    /// - `outbound_url`: Generic Webhook reply URL
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

fn default_true() -> bool {
    true
}

// ── Agent ─────────────────────────────────────────────────────────────────────

/// Sub-agent delegation allowlist configuration.
///
/// ```toml
/// [agents.assistant.subagents]
/// allow = ["coder", "researcher"]   # assistant can delegate to coder & researcher
///
/// [agents.coder.subagents]
/// allow = []   # coder cannot delegate (防递归)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubAgentsConfig {
    /// Agent IDs this agent is permitted to delegate tasks to.
    /// An empty list means **no delegation** is allowed.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Provider name (e.g. "openai", "anthropic", "ollama", "openrouter", "deepseek")
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Model name (e.g. "gpt-4o", "claude-3-5-sonnet-20241022", "llama3")
    pub model: String,
    /// Sampling temperature (0.0–2.0, default 0.7)
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Whitelist of tool names this agent can use (empty = all tools)
    #[serde(default)]
    pub tools: Vec<String>,
    /// Extra instructions appended to the system prompt
    pub system_extra: Option<String>,
    /// Maximum tool call iterations per turn (default: 10)
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Override workspace directory for this agent.
    /// Supports `~` home-dir expansion.
    /// Default: `~/.adaclaw/workspace-{agent_id}`
    pub workspace: Option<String>,
    /// Sub-agent delegation allowlist.
    /// The `delegate` tool is only injected when this list is non-empty.
    #[serde(default)]
    pub subagents: SubAgentsConfig,
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_temperature() -> f64 {
    0.7
}
fn default_max_iterations() -> usize {
    10
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: "gpt-4o".to_string(),
            temperature: default_temperature(),
            tools: Vec::new(),
            system_extra: None,
            max_iterations: default_max_iterations(),
            workspace: None,
            subagents: SubAgentsConfig::default(),
        }
    }
}

// ── Routing ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// Glob pattern to match the channel name (e.g. "telegram:*", "cli")
    pub channel_pattern: Option<String>,
    /// Match by exact sender ID
    pub sender_id: Option<String>,
    /// Glob pattern to match sender display name
    pub sender_name: Option<String>,
    /// Catch-all default rule (matched last)
    #[serde(default)]
    pub default: bool,
    /// Name of the agent to route to (must exist in `agents` map)
    pub agent: String,
}

// ── Security ──────────────────────────────────────────────────────────────────

/// Rate limit configuration (mirrored from `adaclaw_security::ratelimit`
/// to keep `schema.rs` self-contained and avoid a circular dependency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Max inbound messages per user per minute. `0` = unlimited.
    #[serde(default = "default_per_user")]
    pub per_user: u32,
    /// Max inbound messages per channel per minute. `0` = unlimited.
    #[serde(default = "default_per_channel")]
    pub per_channel: u32,
    /// Max tool-call actions per hour. `0` = unlimited.
    #[serde(default = "default_max_actions")]
    pub max_actions_per_hour: u32,
    /// Daily LLM cost budget in USD. `0.0` = unlimited.
    #[serde(default)]
    pub daily_cost_budget_usd: f64,
}

fn default_per_user() -> u32 {
    60
}
fn default_per_channel() -> u32 {
    200
}
fn default_max_actions() -> u32 {
    100 // P2-2 Fix: 统一为 100，与 config.example.toml 保持一致（更保守）
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            per_user: default_per_user(),
            per_channel: default_per_channel(),
            max_actions_per_hour: default_max_actions(),
            daily_cost_budget_usd: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Autonomy level: "readonly" | "supervised" | "full"
    #[serde(default = "default_autonomy")]
    pub autonomy_level: String,
    /// Allowlist of sender IDs / names (deny-by-default when non-empty)
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Allow Full autonomy mode outside a container (risky!)
    #[serde(default)]
    pub allow_full_outside_container: bool,
    /// Workspace root directory (default: ./workspace)
    pub workspace: Option<String>,

    // ── Phase 5 additions ─────────────────────────────────────────────────────

    /// Path to write structured audit events (JSONL). `None` = disabled.
    /// Example: `".adaclaw/audit.jsonl"`
    pub audit_log: Option<String>,

    /// Path to persist emergency-stop state (survives restarts).
    /// Defaults to `".adaclaw/estop.json"`.
    pub estop_state_path: Option<String>,

    /// Require OTP verification when clearing an emergency stop.
    #[serde(default)]
    pub require_otp_for_estop: bool,

    /// In-memory rate limiting configuration.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,

    // ── Phase 11 Round 5: Approval UX ─────────────────────────────────────────

    /// Tools that never require approval in Supervised mode.
    ///
    /// Example: `["file_read", "memory_recall", "memory_store"]`
    ///
    /// These tools always execute automatically regardless of `autonomy_level`,
    /// useful for safe read-only operations that don't need human oversight.
    #[serde(default)]
    pub auto_approve: Vec<String>,

    /// Tools that always require approval, overriding the session allowlist.
    ///
    /// Example: `["shell", "file_write", "http_request"]`
    ///
    /// Even if the user previously approved a tool with "Always", tools in this
    /// list will still prompt every time. Use for high-risk tools.
    #[serde(default)]
    pub always_ask: Vec<String>,

    /// Pending approval request expiry in minutes (default: 30).
    ///
    /// After this timeout, unanswered approval requests are automatically expired
    /// and treated as denied. The user must re-trigger the tool call.
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_minutes: u64,
}

fn default_autonomy() -> String {
    "supervised".to_string()
}

fn default_approval_timeout() -> u64 {
    30
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            autonomy_level: default_autonomy(),
            allowlist: Vec::new(),
            allow_full_outside_container: false,
            workspace: None,
            audit_log: None,
            estop_state_path: None,
            require_otp_for_estop: false,
            rate_limit: RateLimitConfig::default(),
            auto_approve: Vec::new(),
            always_ask: Vec::new(),
            approval_timeout_minutes: default_approval_timeout(),
        }
    }
}

// ── Observability (Phase 7) ───────────────────────────────────────────────────

/// Observability backend configuration.
///
/// ```toml
/// [observability]
/// backend = "prometheus"           # "noop" | "log" | "prometheus"
/// runtime_trace_path = ".adaclaw/runtime-trace.jsonl"
/// runtime_trace_max_entries = 1000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Backend: "noop" | "log" | "prometheus". Default: "noop".
    #[serde(default = "default_obs_backend")]
    pub backend: String,
    /// Path to the runtime trace JSONL file. `None` = disabled.
    pub runtime_trace_path: Option<String>,
    /// Max entries to keep in rolling mode (0 = keep all). Default: 1000.
    #[serde(default = "default_trace_max")]
    pub runtime_trace_max_entries: usize,
}

fn default_obs_backend() -> String {
    "noop".to_string()
}

fn default_trace_max() -> usize {
    1000
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: default_obs_backend(),
            runtime_trace_path: None,
            runtime_trace_max_entries: default_trace_max(),
        }
    }
}

// ── Tunnel (Phase 7) ──────────────────────────────────────────────────────────

/// Tunnel configuration for exposing the gateway to the internet.
///
/// ```toml
/// [tunnel]
/// provider = "cloudflare"
/// cloudflare_token = "eyJhI..."
///
/// # OR
/// provider = "ngrok"
/// ngrok_token = "2abc..."
/// ngrok_domain = "my-agent.ngrok.io"
///
/// # OR
/// provider = "tailscale"
/// tailscale_funnel = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// Tunnel provider: "none" | "cloudflare" | "ngrok" | "tailscale". Default: "none".
    #[serde(default = "default_tunnel_provider")]
    pub provider: String,
    /// Cloudflare Tunnel token (required when provider = "cloudflare").
    pub cloudflare_token: Option<String>,
    /// ngrok auth token (optional, uses pre-configured CLI auth if absent).
    pub ngrok_token: Option<String>,
    /// ngrok custom domain (requires paid ngrok plan).
    pub ngrok_domain: Option<String>,
    /// Use Tailscale Funnel (public internet). False = tailnet-only Serve mode.
    #[serde(default)]
    pub tailscale_funnel: bool,
}

fn default_tunnel_provider() -> String {
    "none".to_string()
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            provider: default_tunnel_provider(),
            cloudflare_token: None,
            ngrok_token: None,
            ngrok_domain: None,
            tailscale_funnel: false,
        }
    }
}

// ── Phase 9：渠道扩展配置（WhatsApp / Email / Matrix）────────────────────────

/// WhatsApp Business Cloud API 渠道配置
///
/// 对应 TOML 中 `[channels.whatsapp]` 块的所有字段的强类型视图。
/// 实际从 `ChannelConfig` 读取（`token` / `webhook_secret` / `extra`）。
///
/// ```toml
/// [channels.whatsapp]
/// kind = "whatsapp"
/// token = "EAA..."                  # Access Token
/// webhook_secret = "..."            # App Secret（可选）
/// allow_from = ["1234567890"]       # 手机号白名单
///
/// [channels.whatsapp.extra]
/// phone_number_id = "12345678"
/// verify_token = "my_verify_token"
/// webhook_port = "9005"             # 独立端口模式（默认）
/// # 或使用 gateway 模式（共享 HTTPS 端口，配合隧道）:
/// # webhook_port 不填，在 server.rs 挂载 WhatsAppRouteState
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WhatsAppConfig {
    /// Graph API Access Token
    pub access_token: String,
    /// Phone Number ID（Meta App Dashboard 中获取）
    pub phone_number_id: String,
    /// Webhook 验证令牌（自定义字符串）
    pub verify_token: String,
    /// App Secret，用于 X-Hub-Signature-256 验证（可选）
    pub app_secret: Option<String>,
    /// 允许的手机号白名单（空 = 放行所有）
    #[serde(default)]
    pub allowed_numbers: Vec<String>,
    /// Webhook 端口（独立模式，默认 9005）
    #[serde(default = "default_whatsapp_port")]
    pub webhook_port: u16,
}

fn default_whatsapp_port() -> u16 {
    9005
}

/// Email 渠道配置（IMAP 收信 + SMTP 发信）
///
/// ```toml
/// [channels.email.extra]
/// consent_granted = "true"          # 必须显式设置（安全门控）
/// imap_host = "imap.gmail.com"
/// imap_port = "993"
/// imap_username = "you@gmail.com"
/// imap_password = "app-password"
/// smtp_host = "smtp.gmail.com"
/// smtp_port = "587"                 # 587=STARTTLS, 465=TLS
/// smtp_username = "you@gmail.com"
/// smtp_password = "app-password"
/// from_address = "Agent <you@gmail.com>"
/// auto_reply_enabled = "true"
/// poll_interval_secs = "60"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailConfig {
    /// IMAP 服务器地址
    pub imap_host: String,
    /// IMAP 端口（默认 993，TLS）
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// IMAP 用户名
    pub imap_username: String,
    /// SMTP 服务器地址
    pub smtp_host: String,
    /// SMTP 端口（587=STARTTLS, 465=TLS）
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// 发件人地址（如 `"Agent <you@gmail.com>"`）
    pub from_address: String,
    /// 是否自动发送回复（false = 只读取）
    #[serde(default = "default_true")]
    pub auto_reply_enabled: bool,
    /// 轮询间隔（秒，默认 60）
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// 发件人白名单（空 = 接受所有）
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// 安全门控（必须显式设为 true）
    #[serde(default)]
    pub consent_granted: bool,
}

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    587
}
fn default_poll_interval() -> u64 {
    60
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: default_imap_port(),
            imap_username: String::new(),
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            from_address: String::new(),
            auto_reply_enabled: true,
            poll_interval_secs: default_poll_interval(),
            allow_from: Vec::new(),
            consent_granted: false,
        }
    }
}

/// Matrix 渠道配置（Client-Server API）
///
/// 需要编译时开启 `feature = "matrix"`。
///
/// ```toml
/// [channels.matrix]
/// kind = "matrix"
/// token = "syt_..."                 # access_token
/// allow_from = ["@admin:matrix.org", "!roomid:matrix.org"]
///
/// [channels.matrix.extra]
/// homeserver = "https://matrix.org"
/// user_id = "@mybot:matrix.org"
/// device_id = "ADACLAWDEV01"
/// sync_timeout_ms = "30000"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Matrix homeserver URL（如 `https://matrix.org`）
    pub homeserver: String,
    /// Bot 用户 ID（如 `@bot:matrix.org`）
    pub user_id: String,
    /// 访问令牌（Matrix access_token）
    pub access_token: String,
    /// 设备 ID（稳定跨重启）
    #[serde(default = "default_device_id")]
    pub device_id: String,
    /// 允许的用户 ID 或 room ID 白名单
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// 长轮询超时毫秒数（默认 30000）
    #[serde(default = "default_sync_timeout")]
    pub sync_timeout_ms: u64,
    /// 是否启用 E2EE（需要 `vodozemac` 扩展，当前版本不支持）
    #[serde(default)]
    pub e2ee_enabled: bool,
}

fn default_device_id() -> String {
    "ADACLAW".to_string()
}
fn default_sync_timeout() -> u64 {
    30_000
}

impl Default for MatrixConfig {
    fn default() -> Self {
        Self {
            homeserver: String::new(),
            user_id: String::new(),
            access_token: String::new(),
            device_id: default_device_id(),
            allow_from: Vec::new(),
            sync_timeout_ms: default_sync_timeout(),
            e2ee_enabled: false,
        }
    }
}

// ── Phase 10：生态对接配置（MCP / Heartbeat / Groq）─────────────────────────

/// 工具扩展配置（Phase 10 MCP）
///
/// ```toml
/// [tools.mcp_servers.filesystem]
/// command = "npx"
/// args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
///
/// [tools.mcp_servers.my-remote]
/// url     = "https://example.com/mcp/"
/// headers = { Authorization = "Bearer xxx" }
/// tool_timeout = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    /// MCP Server 配置（key = server 逻辑名，value = Stdio 或 HTTP 配置）
    #[serde(default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
}

/// MCP Server 配置（与 Claude Desktop / nanobot 格式兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// Stdio transport：启动本地进程
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        tool_timeout: Option<u64>,
    },
    /// HTTP transport：连接远程 MCP Server
    Http {
        url: String,
        #[serde(default)]
        headers: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        tool_timeout: Option<u64>,
    },
}

/// Heartbeat（主动任务）配置（Phase 10）
///
/// ```toml
/// [heartbeat]
/// enabled = true
/// interval_minutes = 30
/// target_channel = "telegram"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    /// 是否启用 Heartbeat（默认 false）
    #[serde(default)]
    pub enabled: bool,
    /// 执行间隔（分钟，默认 30，最小 5）
    #[serde(default = "default_heartbeat_interval")]
    pub interval_minutes: u64,
    /// 结果回传渠道名（如 "telegram"，空 = 最近活跃渠道）
    pub target_channel: Option<String>,
    /// Heartbeat 文件路径（默认 workspace/HEARTBEAT.md）
    pub heartbeat_file: Option<String>,
}

fn default_heartbeat_interval() -> u64 {
    30
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: default_heartbeat_interval(),
            target_channel: None,
            heartbeat_file: None,
        }
    }
}

// ── Gateway ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Bind address (default: 127.0.0.1:8080)
    #[serde(default = "default_gateway_bind")]
    pub bind: String,
    /// Bearer token for API authentication (required in production)
    pub bearer_token: Option<String>,
    /// Enable CORS (default: false — only enable for WebUI)
    #[serde(default)]
    pub cors_enabled: bool,
}

fn default_gateway_bind() -> String {
    "127.0.0.1:8080".to_string()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: default_gateway_bind(),
            bearer_token: None,
            cors_enabled: false,
        }
    }
}
