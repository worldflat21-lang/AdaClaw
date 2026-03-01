# AdaClaw — Rust AI Agent Runtime

> 轻量、安全、多渠道、多 Agent 的开源 Rust AI Agent Runtime
>
> 对标：zeroclaw / picoclaw / nanobot / openclaw
>
> 目标：二进制 <10MB，内存 <5MB，启动 <50ms

---

## 项目定位

AdaClaw 是一个面向技术用户的开源 AI Agent Runtime，用 Rust 编写，兼顾极致性能与生产级安全。核心理念：

- **所有主干组件 Trait 化** —— Provider / Channel / Memory / Tool / Security 全部抽象为 trait，实现可随时替换
- **数据驱动扩展点** —— 新增 Provider/Channel 只需注册一条 ProviderSpec，不改逻辑代码
- **消息流单向总线** —— Channel 与 Agent 完全解耦，通过 MessageBus 通信
- **纵深安全防御** —— 7 层安全体系，任何一层被绕过不至于全线崩溃
- **国内渠道一等公民** —— 钉钉、飞书、企业微信与 Telegram/Discord 同等支持
- **MCP 原生支持** —— 内置 Model Context Protocol 客户端，任意外部 MCP Server 动态接入工具生态

---

## 一、整体架构图

```
┌──────────────────────────────────────────────────────────────────┐
│  CLI (clap)            Onboard Wizard          Doctor            │
│  adaclaw run / adaclaw chat / adaclaw config / adaclaw stop / adaclaw status         │
└──────────────────────────┬───────────────────────────────────────┘
                           │
┌──────────────────────────▼───────────────────────────────────────┐
│  Daemon                                                           │
│  ┌──────────────┐  ┌──────────────┐  ┌─────────────────────────┐ │
│  │   Gateway    │  │  Scheduler   │  │  Heartbeat / Watchdog   │ │
│  │  (axum HTTP) │  │  (cron/at)   │  │                         │ │
│  └──────┬───────┘  └──────┬───────┘  └─────────────────────────┘ │
│         │                 │                                       │
│  ┌──────▼─────────────────▼──────────────────────────────────┐   │
│  │  Channel Manager                                           │   │
│  │  Telegram│Discord│Slack│DingTalk│Feishu│WeChat│Email│...  │   │
│  └────────────────────────┬───────────────────────────────────┘   │
│                           │  InboundMessage                       │
│  ┌────────────────────────▼───────────────────────────────────┐   │
│  │  Message Bus (tokio::sync::mpsc + broadcast)               │   │
│  │  ┌──────────────────────────────────────────────────────┐  │   │
│  │  │  Agent Router ← RoutingRule[] ← AgentRegistry        │  │   │
│  │  └──────────────────────────────────────────────────────┘  │   │
│  └────────────────────────┬───────────────────────────────────┘   │
│                           │  AgentTask                            │
│  ┌────────────────────────▼───────────────────────────────────┐   │
│  │  Agent Engine                                              │   │
│  │  ┌──────────────┐  ┌────────────────┐  ┌───────────────┐  │   │
│  │  │ Context Mgr  │  │  Tool Call     │  │   History     │  │   │
│  │  │ (Memory RAG) │  │  Loop (并行/   │  │   Compactor   │  │   │
│  │  │              │  │  去重/多格式)  │  │   (LLM摘要)   │  │   │
│  │  └──────────────┘  └────────────────┘  └───────────────┘  │   │
│  └────────────────┬──────────────────────────────────────────┘   │
│                   │                                               │
│  ┌────────────────▼──────────────────────────────────────────┐   │
│  │  Infrastructure Layer                                      │   │
│  │  ┌──────────────┐  ┌──────────────┐  ┌─────────────────┐  │   │
│  │  │  Provider    │  │  Tool        │  │  Memory Engine  │  │   │
│  │  │  Router      │  │  Registry    │  │  sqlite-vec     │  │   │
│  │  │  Reliability │  │  shell/file/ │  │  FTS5 + RRF     │  │   │
│  │  │  Chain       │  │  http/memory │  │  fastembed      │  │   │
│  │  └──────────────┘  └──────────────┘  └─────────────────┘  │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │  Security Layer (7层纵深防御)                              │   │
│  │  Pairing│Allowlist│Sandbox│Estop│OTP│Scrub│Audit│RateLimit│   │
│  └───────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

---

## 二、目录结构

```
adaclaw/
├── Cargo.toml                   # workspace root
├── Cargo.lock
├── .cargo/
│   └── config.toml              # release profile: opt-level="z", lto, strip
├── ARCHITECTURE.md              # 本文件
├── README.md
├── LICENSE                      # Apache-2.0（单一许可证文件）
├── Dockerfile                   # 多阶段构建（builder + debian:bookworm-slim runtime）
├── docker-compose.yml
│
├── crates/                      # 可复用 library crates
│   ├── adaclaw-core/                # 核心 trait 定义（零实现，最小依赖）
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── provider.rs      # Provider trait + ChatMessage + ChatResponse
│   │       ├── channel.rs       # Channel trait + InboundMessage + OutboundMessage
│   │       ├── memory.rs        # Memory trait + MemoryEntry + MemoryCategory
│   │       ├── tool.rs          # Tool trait + ToolSpec + ToolResult
│   │       ├── observer.rs      # Observer trait + ObserverEvent
│   │       ├── sandbox.rs       # Sandbox trait
│   │       └── tunnel.rs        # Tunnel trait
│   │
│   ├── adaclaw-providers/           # LLM 提供商实现
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── registry.rs      # ProviderSpec 数据驱动注册表
│   │       ├── router.rs        # create_provider() 工厂函数
│   │       ├── reliable.rs      # ReliabilityChain (故障切换)
│   │       ├── openai.rs        # OpenAI + 所有兼容端点
│   │       ├── anthropic.rs     # Anthropic Claude
│   │       ├── openrouter.rs    # OpenRouter — 单 key 访问数百模型
│   │       ├── deepseek.rs      # DeepSeek (deepseek-chat / deepseek-reasoner)
│   │       ├── ollama.rs        # Ollama (本地推理)
│   │       └── openai_compat.rs # 通用 OpenAI-compatible 端点
│   │
│   ├── adaclaw-channels/            # 渠道实现
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── base.rs          # BaseChannel 辅助结构（白名单/运行状态/消息上报）
│   │       ├── manager.rs       # ChannelManager（并发管理 + Outbound Dispatch Loop）
│   │       ├── telegram.rs      # Telegram Bot API（长轮询+Webhook，HMAC验证）
│   │       ├── discord.rs       # Discord（Gateway WebSocket + 心跳分离 + 指数退避重连）
│   │       ├── slack.rs         # Slack（Events API Webhook，HMAC+防重放）
│   │       ├── dingtalk.rs      # 钉钉（Outgoing Webhook，HMAC-SHA256）
│   │       ├── feishu.rs        # 飞书/Lark（事件订阅，tenant_access_token 自动刷新）
│   │       ├── wechat_work.rs   # 企业微信（SHA1验证 + AES-256-CBC block_size=32 解密）
│   │       ├── webhook.rs       # 通用 HTTP Webhook（HMAC可选，标准JSON入站格式）
│   │       ├── whatsapp.rs      # WhatsApp Business Cloud API（Meta Webhook）
│   │       ├── email.rs         # Email（IMAP+SMTP，async-imap + lettre）
│   │       ├── matrix.rs        # Matrix（Client-Server API，E2EE 可选，feature = "matrix"）
│   │       └── cli.rs           # 本地 CLI 渠道（交互式 REPL）
│   │
│   ├── adaclaw-memory/              # 记忆后端
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── factory.rs       # create_memory() 工厂
│   │       ├── sqlite.rs        # SQLite + FTS5 + sqlite-vec + RRF 混合检索
│   │       ├── markdown.rs      # Markdown 文件存储（YAML front-matter）
│   │       ├── none.rs          # 显式禁用（no-op backend）
│   │       ├── global.rs        # GlobalMemory wrapper（全局共享知识库）
│   │       ├── consolidation.rs # 记忆刷写整理（两阶段LLM去重合并）
│   │       ├── query.rs         # QMD 查询分解（复杂查询→子查询→N路RRF）
│   │       ├── topic.rs         # Topic 摘要辅助
│   │       ├── session_store.rs # 会话存储
│   │       ├── embeddings/
│   │       │   ├── mod.rs       # EmbeddingProvider trait
│   │       │   ├── fastembed.rs # 本地推理（AllMiniLML6V2，384维，spawn_blocking）
│   │       │   ├── openai.rs    # OpenAI text-embedding-3-small
│   │       │   └── none.rs      # 无嵌入（降级为纯FTS5）
│   │       └── rrf.rs           # Reciprocal Rank Fusion 算法（k=60，独立模块）
│   │
│   ├── adaclaw-tools/               # 工具实现
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── registry.rs      # 工具注册 + all_tools()
│   │       ├── shell.rs         # shell 命令执行
│   │       ├── file.rs          # file_read / write / list / edit
│   │       ├── memory_tools.rs  # memory_store / recall / forget
│   │       ├── http.rs          # http_request
│   │       └── mcp/             # MCP 客户端（McpClient + McpTool，stdio + HTTP/SSE transport）
│   │           ├── mod.rs
│   │           ├── loader.rs
│   │           ├── http.rs
│   │           └── stdio.rs
│   │   # cron_tools.rs 工具形式调用 cron（cron 逻辑在 src/cron/ 主二进制中）
│   │
│   │   注：Agent 间委托工具（DelegateTool）在 src/agents/delegate.rs，不在此 crate
│   │
│   ├── adaclaw-security/            # 安全模块
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── sandbox.rs       # Sandbox 模块入口（pub mod workspace/landlock/docker）
│   │       ├── sandbox/
│   │       │   ├── workspace.rs # 路径隔离 + 符号链接检测 + 系统目录黑名单
│   │       │   ├── landlock.rs  # Linux Landlock LSM（非Linux优雅降级）
│   │       │   └── docker.rs    # 容器环境感知（is_running_in_container）
│   │       ├── estop.rs         # 紧急停止 (4级：KillAll/NetworkKill/DomainBlock/ToolFreeze)
│   │       ├── otp.rs           # TOTP (RFC 6238，HMAC-SHA1，无外部依赖)
│   │       ├── secrets.rs       # ChaCha20-Poly1305 加密存储
│   │       ├── scrub.rs         # 凭证脱敏（26种模式正则）
│   │       ├── ratelimit.rs     # 速率限制（per_user/channel/cost/actions）
│   │       ├── audit.rs         # 结构化审计日志（JSONL，可接 SIEM）
│   │       ├── ssrf.rs          # SSRF 防护
│   │       └── approval.rs      # 工具执行审批（AutonomyLevel: ReadOnly/Supervised/Full）
│   │
│   │   注：Gateway 配对码（Pairing）实现在 crates/adaclaw-server/src/pairing.rs
│   │
│   └── adaclaw-server/              # Gateway HTTP 服务
│       └── src/
│           ├── lib.rs
│           ├── server.rs        # axum 服务器启动
│           ├── routes/
│           │   ├── mod.rs       # 路由注册
│           │   ├── chat.rs      # POST /v1/chat
│           │   ├── status.rs    # GET  /v1/status
│           │   ├── stop.rs      # POST /v1/stop (Estop)
│           │   ├── whatsapp.rs  # WhatsApp Webhook 回调
│           │   └── metrics.rs   # GET /metrics (Prometheus)
│           ├── pairing.rs       # GET /pair (配对码)
│           └── middleware.rs    # Auth / RateLimit / CORS
│           # ↓ 规划中（未实现）
│           # ws.rs              WebSocket 双向流
│           # routes/memory.rs   GET/POST/DELETE /v1/memory
│
└── src/                         # 主二进制 crate
    ├── main.rs
    ├── config/
    │   ├── mod.rs               # 配置加载入口（toml + 环境变量覆盖）
    │   ├── schema.rs            # 完整配置结构体（Config + 所有子配置）
    │   ├── validation.rs        # 配置校验
    │   └── migration.rs         # 配置迁移
    ├── bus/
    │   ├── mod.rs               # MessageBus（mpsc sender + broadcast sender）
    │   ├── router.rs            # AgentRouter + RoutingRule（3级优先级）
    │   └── queue.rs             # Bus 实例 + send_inbound_bypass()
    │   # 注：InboundMessage/OutboundMessage 类型定义在 mod.rs 中
    ├── agents/
    │   ├── mod.rs
    │   ├── registry.rs          # AgentRegistry（批量创建 AgentInstance）
    │   ├── instance.rs          # AgentInstance 生命周期（per-agent workspace/tools）
    │   ├── engine.rs            # Tool Call Loop（核心：并行执行/去重/多格式解析）
    │   ├── parser.rs            # 工具调用多格式解析器（OpenAI/XML/Markdown/GLM）
    │   ├── compact.rs           # Congee 滚动摘要（rolling_compact）
    │   ├── message_tool.rs      # 消息发送工具
    │   └── delegate.rs          # DelegateTool（异步 Agent 间任务委托）
    ├── daemon/
    │   ├── mod.rs
    │   ├── run.rs               # 守护进程主循环（集成所有子系统）
    │   └── reload.rs            # 热重载
    ├── cli/
    │   ├── mod.rs               # CLI 入口（clap 子命令分发）
    │   ├── doctor.rs            # adaclaw doctor（全系统诊断）
    │   ├── onboard.rs           # adaclaw onboard（引导向导）
    │   └── skill.rs             # adaclaw skill（技能管理）
    │   # 注：chat/run/status/stop/config 等命令在 mod.rs 或 main.rs 中直接实现
    ├── observability/
    │   ├── mod.rs               # create_observer() 工厂 + 全局 observer 单例
    │   ├── noop.rs              # 零开销 noop 观察者
    │   ├── log.rs               # tracing 日志观察者
    │   ├── prometheus.rs        # Prometheus 指标（纯 atomic，8类指标）
    │   └── trace.rs             # RuntimeTracer（结构化运行时事件，JSONL）
    │   # 注：OpenTelemetry（otel.rs）规划中，feature-gated
    ├── skills/
    │   ├── mod.rs
    │   └── loader.rs            # SKILL.md 加载
    ├── identity/
    │   ├── mod.rs
    │   └── loader.rs            # IDENTITY.md / 配置
    ├── state/
    │   └── mod.rs               # 运行时状态
    ├── cron/
    │   ├── mod.rs
    │   └── scheduler.rs
    └── tunnel/
        ├── mod.rs
        ├── cloudflare.rs
        ├── tailscale.rs
        └── ngrok.rs
```

---

## 三、核心 Trait 设计

### Provider Trait

```rust
// adaclaw-core/src/provider.rs
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn supports_native_tools(&self) -> bool { self.capabilities().native_tool_calling }
    fn supports_vision(&self) -> bool { self.capabilities().vision }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse>;
    async fn chat_with_system(&self, system: Option<&str>, msg: &str, model: &str, temp: f64) -> Result<String>;
    async fn warmup(&self) -> Result<()> { Ok(()) }
}

pub struct ProviderCapabilities {
    pub native_tool_calling: bool,
    pub vision: bool,
    pub streaming: bool,
}

// 数据驱动注册表 (adaclaw-providers/src/registry.rs)
pub struct ProviderSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub local: bool,
    pub capabilities: ProviderCapabilities,
    pub factory: fn(key: Option<&str>, url: Option<&str>) -> Box<dyn Provider>,
}
```

### Channel Trait

```rust
// adaclaw-core/src/channel.rs
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, bus: Arc<MessageBus>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
    async fn stop(&self) -> Result<()>;
}

pub struct InboundMessage {
    pub id: Uuid,
    pub channel: String,       // "telegram:@mybot"
    pub session_id: String,    // 对话会话 ID
    pub sender_id: String,     // 用户唯一标识
    pub sender_name: String,
    pub content: MessageContent, // Text / Image / Audio / File
    pub reply_to: Option<Uuid>,
    pub metadata: HashMap<String, Value>,
}
```

### Memory Trait

```rust
// adaclaw-core/src/memory.rs
#[async_trait]
pub trait Memory: Send + Sync {
    fn name(&self) -> &str;
    async fn store(&self, key: &str, content: &str, category: Category, session: Option<&str>) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize, session: Option<&str>) -> Result<Vec<MemoryEntry>>;
    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>>;
    async fn list(&self, category: Option<&Category>, session: Option<&str>) -> Result<Vec<MemoryEntry>>;
    async fn forget(&self, key: &str) -> Result<bool>;
    async fn count(&self) -> Result<usize>;
    async fn health_check(&self) -> bool;
}

pub enum Category {
    Core,         // 长期事实，用户明确保留
    Daily,        // 短期工作笔记
    Conversation, // AgentEngine 自动写入的对话索引
    Global,       // 所有 Agent 共享的只读参考知识
    Custom(String),
}
```

### Tool Trait

```rust
// adaclaw-core/src/tool.rs
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value) -> Result<ToolResult>;
}

pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}
```

### MCP 客户端设计（adaclaw-tools/src/mcp/）

MCP Client 把外部 MCP Server 暴露的工具**透明包装为 `Tool` trait 实现**，与原生工具完全同等对待：

```rust
// adaclaw-tools/src/mcp/mod.rs

/// MCP Server 配置（与 Claude Desktop / nanobot 格式兼容）
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// Stdio transport：本地进程（npx / uvx / 可执行文件）
    Stdio { command: String, args: Vec<String>, env: Option<HashMap<String, String>> },
    /// HTTP/SSE transport：远程 Server
    Http  { url: String, headers: Option<HashMap<String, String>>, tool_timeout: Option<u64> },
}

/// 将一个 MCP tool 包装为 Tool trait
pub struct McpTool {
    pub server_name: String,
    pub tool_name:   String,
    pub description: String,
    pub schema:      Value,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str { &self.tool_name }
    fn description(&self) -> &str { &self.description }
    fn parameters_schema(&self) -> Value { self.schema.clone() }
    fn spec(&self) -> ToolSpec { ToolSpec { name: self.tool_name.clone(), .. } }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        self.client.call_tool(&self.tool_name, args).await
    }
}
```

MCP 配置（`config.toml` 中，与 Claude Desktop/nanobot 格式兼容）：

```toml
[tools.mcp_servers.filesystem]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]

[tools.mcp_servers.my-remote-mcp]
url     = "https://example.com/mcp/"
headers = { Authorization = "Bearer xxxxx" }
tool_timeout = 30   # 秒，默认 30
```

---

## 四、消息总线设计

```
Channel A ─┐
Channel B ─┼──→ MessageBus (mpsc) ──→ AgentRouter ──→ Agent "assistant"
Channel C ─┘                    ↘──→ AgentRouter ──→ Agent "coder"
                                                ↓
                                    OutboundMessage (broadcast)
                                                ↓
                             ┌──────────────────┴──────────────────┐
                         Channel A                             Channel B
```

路由规则（配置驱动）：
```toml
[[routing]]
channel_pattern = "telegram:@dev_bot"
agent = "coder"

[[routing]]
sender_id = "user_12345"
agent = "assistant"

[[routing]]
default = true
agent = "assistant"
```

---

## 五、运行环境策略

AdaClaw 支持两种运行模式，与 `AutonomyLevel`（自治级别）联动，共同构成"部署环境 × 权限级别"的二维安全矩阵。

### 5.1 运行模式

#### 模式一：直接本地运行（默认，最轻量）

用户直接运行编译好的二进制文件（Windows `.exe`，macOS/Linux ELF）：

```
adaclaw run          # 前台运行
adaclaw daemon start # 守护进程模式
```

- **不依赖任何外部环境**，启动极快（目标 <50ms），内存占用极低（目标 <5MB）
- 系统默认 `AutonomyLevel = Supervised`（学徒模式）：
  - **CLI 渠道**：工具执行前显示确认框（`y/N`），需要用户手动确认
  - **消息渠道（Telegram/Discord 等）**：工具调用被**静默拒绝**，不打断用户，但 AI 无法执行操作。需改为 `Full` 才能在消息渠道中执行工具
  - 个人用户建议配置 `autonomy_level = "full"` 以获得完整功能和无打断体验
- 适合：日常使用、开发调试、`ReadOnly` / `Supervised` 级别场景

#### 模式二：Docker 沙箱运行（可选，高级安全模式）

仓库提供 `docker-compose.yml` 模板，用户一键启动：

```
docker compose up -d
```

- AI 的所有操作（文件读写、shell 执行）**被限制在容器内部**，完全无法触达宿主机文件系统
- 即使 AI 行为失控（误删文件、执行危险命令），破坏范围也被严格限定在容器沙箱内
- 适合：`Full` 自治模式（100% 自动执行，无人工介入）的生产场景

### 5.2 运行环境 × 自治级别 矩阵

```
                  ReadOnly       Supervised       Full
                （观察者模式）  （学徒模式）   （专家模式）
                ─────────────────────────────────────────────
直接本地运行      ✅ 推荐         ✅ 默认        ⚠️  警告，强烈建议改用 Docker
Docker 容器       ✅              ✅             ✅  推荐，隔离宿主机
```

### 5.3 Full 模式保护机制

当用户配置 `AutonomyLevel = Full` 时，AdaClaw 在启动时执行以下检查：

1. **容器环境检测**：
   - Linux：检查 `/.dockerenv` 是否存在 + 读取 `/proc/1/cgroup` 判断 cgroup 类型
   - macOS/Windows：检查已知容器环境变量（`DOCKER_CONTAINER`、`container` 等）

2. **非容器环境下的行为**：
   - 打印醒目警告（stderr，带颜色），说明风险
   - `adaclaw doctor` 输出 `WARN: Full mode outside container`
   - **不强制阻止**：用户可通过配置 `security.allow_full_outside_container = true` 或命令行 `--i-know-what-i-am-doing` 显式跳过

3. **`adaclaw onboard` 引导**：
   - 当用户在向导中选择 `Full` 模式时，自动展示 Docker 安装指引
   - 询问是否生成 `docker-compose.yml`

### 5.4 `docker.rs` 职责定义

```rust
// adaclaw-security/src/sandbox/docker.rs

/// 容器环境感知（被动检测，不主动创建容器）
pub struct ContainerEnvironment;

impl ContainerEnvironment {
    /// 检测当前进程是否运行在 Docker/OCI 容器内
    /// Linux: 检查 /.dockerenv + /proc/1/cgroup
    /// macOS/Windows: 检查环境变量
    pub fn is_running_in_container() -> bool { ... }

    /// Full 模式下，若不在容器内，返回安全警告
    /// 返回 None 表示环境安全（在容器内），Some(warn) 表示需要提示用户
    pub fn check_autonomy_safety(level: &AutonomyLevel) -> Option<SecurityWarning> { ... }
}

pub struct SecurityWarning {
    pub level: WarnLevel,    // Warn / Critical
    pub message: String,
    pub mitigation: String,  // "Run with docker compose up -d"
}
```

### 5.5 docker-compose.yml 模板（仓库提供）

```yaml
# docker-compose.yml（仓库根目录）
# 推荐在 Full 自治模式下使用
version: "3.9"
services:
  adaclaw:
    image: ghcr.io/your-org/adaclaw:latest
    # 或本地构建：
    # build: .
    restart: unless-stopped
    environment:
      - ADACLAW_AUTONOMY_LEVEL=full
      - ADACLAW_API_KEY=${ADACLAW_API_KEY}
    volumes:
      # 仅挂载 AdaClaw 的工作目录，宿主机其他目录完全隔离
      - ./workspace:/app/workspace
      - ./config.toml:/app/config.toml:ro
    ports:
      # Gateway 只绑定本地，通过 Cloudflare/Tailscale 隧道对外暴露
      - "127.0.0.1:8080:8080"
    # 安全加固
    read_only: true          # 容器根文件系统只读
    tmpfs:
      - /tmp                 # 临时目录可写
    cap_drop:
      - ALL                  # 删除所有 Linux capabilities
    security_opt:
      - no-new-privileges:true
```

---

## 六、安全体系（7层纵深防御）

```
第1层 网络边界    Gateway 默认 127.0.0.1，公网必须配隧道
第2层 渠道认证    Pairing 配对码 + Bearer Token，Webhook HMAC 验证
第3层 用户白名单  Channel allowlist deny-by-default
第4层 工具审批    AutonomyLevel: readonly/supervised/full（联动运行环境策略，见第五节）
第5层 文件系统    workspace 隔离 + 黑名单 + 符号链接检测 + Landlock + Docker 容器
第6层 输出脱敏    scrub_credentials() 凭证自动脱敏
第7层 紧急停止    Estop 4级 (KillAll/NetworkKill/DomainBlock/ToolFreeze) + OTP
```

附加：
- 密钥加密存储（ChaCha20-Poly1305）
- 速率限制（per_user / per_channel / daily_cost_budget）
- 结构化审计日志（JSONL，可接 SIEM）
- Prompt Injection 防护（工具调用解析严格边界）

---

## 七、记忆检索架构（RRF 混合）

```
用户消息 "帮我查上次的部署决定"
    │
    ├──→ FTS5 关键词检索 (BM25 排名) ──→ [Entry A: rank 1, Entry C: rank 3]
    │
    └──→ 向量语义检索 (sqlite-vec)  ──→ [Entry B: rank 1, Entry A: rank 2]
                │
                └──→ RRF 融合 (k=60)
                        Entry A: 1/(60+1) + 1/(60+2) = 最终分最高
                        Entry B: 0 + 1/(60+1)
                        Entry C: 1/(60+3) + 0
                        └──→ 返回 Top-K 相关记忆注入上下文
```

EmbeddingProvider 优先级：
1. fastembed（本地，零 API，AllMiniLML6V2，384维）
2. OpenAI text-embedding-3-small
3. None（降级为纯 FTS5 关键词检索）

---

## 八、技术选型

| 模块 | 依赖 | 理由 |
|------|------|------|
| 异步运行时 | tokio | 生态最成熟 |
| HTTP 框架 | axum | 性能强，与 tokio 原生 |
| CLI | clap (derive) | 最成熟 |
| 序列化 | serde + toml + serde_json | 标准 |
| HTTP 客户端 | reqwest (stream) | 异步流式 |
| SQLite | rusqlite | 内置 FTS5 |
| 向量检索 | sqlite-vec | 轻量，与 rusqlite 集成 |
| 本地嵌入 | fastembed | 零 API 依赖，本地推理 |
| 加密 | chacha20poly1305 | AEAD，轻量 |
| OTP | totp-rs | TOTP/RFC 6238 |
| 正则 | regex + std::sync::LazyLock | 高性能，延迟编译 |
| 错误处理 | anyhow + thiserror | 标准组合 |
| 结构化日志 | tracing + tracing-subscriber | 生产级 |
| 指标 | metrics + metrics-exporter-prometheus | 轻量 |
| 链路追踪 | opentelemetry | 可选，feature-gated |
| UUID | uuid (v4) | 标准 |
| 时间 | chrono | 序列化友好 |
| 并发 | futures-util (join_all) | 并行工具执行 |
| 取消 | tokio-util (CancellationToken) | Agent 取消 |
| MCP 客户端 | rmcp | 官方 Rust MCP SDK，stdio + HTTP/SSE transport |

**Release profile 优化（目标 <10MB）：**
```toml
[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"
```

---

## 九、对标竞品差异化

| 特性 | **AdaClaw** | zeroclaw | picoclaw | nanobot | openclaw |
|------|---------|---------|---------|---------|---------|
| 语言 | **Rust** | Rust | Go | Python | TypeScript |
| 国内渠道 | **✅ 钉钉/飞书/企微** | 部分 | 部分 | ❌ | 部分 |
| 本地嵌入 RRF | **✅ fastembed** | ✅ | ❌ | ❌ | ❌ |
| 多 Agent 路由 | **✅ 配置驱动** | ❌ 无 | ✅ 7级优先 | ❌ 无 | ✅ session 式 |
| Agent 委托 | **✅ 异步 delegate** | ❌ | ✅ spawn | ✅ spawn | ✅ sessions-spawn |
| 安全纵深 7层 | **✅** | 部分(4项) | 部分 | 基础 | 部分 |
| Provider 注册表 | **✅ 数据驱动** | 部分 | 部分 | ✅ | ❌ |
| Provider 熔断退避 | **✅ 指数退避+熔断** | ❌ | ✅ CooldownTracker | ❌ | ❌ |
| 消息总线解耦 | **✅** | ❌ | ❌ | ✅ | ✅ |
| MCP 工具协议 | **✅ 原生支持** | ❌ | ❌ | ✅ | ❌ |
| 二进制大小 | **<10MB** | <9MB | ~8MB | N/A | ~28MB |
| Web UI | **规划** | ❌ | ❌ | ❌ | ✅ |

---

*最后更新：2026-03-01*
