<div align="center">
  <img src="assets/Ada103iow103.png" width="80" alt="AdaClaw">
  <h1>AdaClaw ⚡</h1>
  <p><strong>轻量 · 安全 · 多渠道 · 多 Agent 的 Rust AI Agent 运行时</strong></p>
  <p>
    <a href="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml">
      <img src="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="https://github.com/worldflat21-lang/AdaClaw/releases">
      <img src="https://img.shields.io/github/v/release/worldflat21-lang/AdaClaw?style=flat" alt="Release">
    </a>
    <img src="https://img.shields.io/badge/binary-%3C10MB-brightgreen?style=flat" alt="Binary <10MB">
    <img src="https://img.shields.io/badge/RAM-%3C5MB-brightgreen?style=flat" alt="RAM <5MB">
    <img src="https://img.shields.io/badge/startup-%3C50ms-brightgreen?style=flat" alt="Startup <50ms">
    <img src="https://img.shields.io/badge/license-Apache--2.0-blue?style=flat" alt="Apache 2.0">
    <img src="https://img.shields.io/badge/rust-stable-orange?style=flat" alt="Rust stable">
  </p>
  <p><a href="README.md">English</a></p>
</div>

AdaClaw 是一个开源 **AI Agent 运行时**——单个 Rust 二进制文件，将你选择的大模型与你日常使用的渠道（Telegram、Discord、Slack、钉钉、飞书、企业微信等）连接起来，内置混合记忆引擎和 7 层安全体系。

部署在自己的机器、树莓派，或者容器里。除大模型 API 调用本身，无任何额外费用；所有数据留在本地，不做任何回传。

---

## 产品对比

|  | OpenClaw | NanoBot | PicoClaw | ZeroClaw | **AdaClaw** |
|--|--|--|--|--|--|
| **语言** | TypeScript | Python | Go | Rust | **Rust** |
| **内存** | > 1 GB | > 100 MB | < 10 MB | < 5 MB | **< 5 MB** |
| **启动时间** | > 500 s | > 30 s | < 1 s | < 10 ms | **< 50 ms** |
| **Multi-Agent** | ✅ | ✅ | ✅ | ❌ | ✅ 配置驱动 + 异步委托 |
| **MCP** | ❌ | ✅ stdio + HTTP | ❌ | ❌ | ✅ stdio + HTTP/SSE |
| **RRF 混合记忆** | ❌ | ❌ | ❌ | ✅ | ✅ FTS5 + 向量 + 本地 Embed |
| **安全层数** | DM pairing | 基础 | workspace | 4 层 | **7 层** |
| **Provider 熔断** | ✅ | ❌ | ✅ | ❌ | ✅ circuit breaker |
| **ARM / 树莓派** | ❌ | 部分 | ✅ | ✅ | ✅ |

> 启动时间基准：0.8 GHz 单核设备；AdaClaw 使用 `--release` + `opt-level = "z"` 构建。

---

## 整体架构

```
 渠道层  Telegram · Discord · Slack · 钉钉 · 飞书 · 企业微信 · Webhook · CLI
      │
      ▼
 ┌──────────────────────────────────────────────────┐
 │                 Message Bus                      │
 │      mpsc（点对点）  +  broadcast（广播）          │
 └───────────┬──────────────────────┬───────────────┘
             │                      │
       ┌─────▼──────┐        ┌──────▼──────┐
       │   Agent    │◄─派发──│  Sub-Agent  │
       │   Engine   │        │  （异步）    │
       └─────┬──────┘        └─────────────┘
             │
  ┌──────────┼─────────────────────┐
  │          │                     │
┌─▼────┐  ┌──▼─────┐  ┌───────────▼──────────────┐
│Memory│  │ Tools  │  │  Security Layer（7 层）    │
│ RRF  │  │ + MCP  │  │ 网络→鉴权→白名单→沙箱      │
└──────┘  └────────┘  │  →estop→脱敏→审计          │
                       └──────────────────────────┘
             │
       ┌─────▼──────┐
       │  Providers │  ReliabilityChain · circuit breaker
       │  OpenAI · Anthropic · DeepSeek · Ollama · …
       └────────────┘
```

---

## 快速开始

```bash
# Linux / macOS — 一行安装
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
adaclaw onboard   # 交互式向导：API Key → 渠道 → 工作区
adaclaw chat
```

```powershell
# Windows
irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
adaclaw onboard
adaclaw chat
```

从源码构建：

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo build --release
./target/release/adaclaw onboard
```

---

## 安装方式

| 平台 | 安装方式 |
|--|--|
| Linux / macOS | `curl -fsSL https://…/install.sh \| bash` |
| Windows（PowerShell） | `irm https://…/install.ps1 \| iex` |
| macOS Homebrew | `brew tap worldflat21-lang/adaclaw && brew install adaclaw` |
| 任意平台 | `cargo install --git https://github.com/worldflat21-lang/AdaClaw` |

每个 Release 发布 `x86_64`、`aarch64`（ARM64）、`armv7` 预编译二进制，支持 Linux、macOS 及树莓派。

### Docker

在 `autonomy_level = "full"` 场景下推荐使用 Docker：

```bash
cp config.example.toml config.toml
# 填入 API Key 等配置项
docker compose up -d
docker compose logs -f
```

内置 `docker-compose.yml` 已加固：只读根文件系统、降权（`cap_drop=ALL`）、`tmpfs /tmp`、仅监听 `127.0.0.1`。

---

## 渠道接入

| 渠道 | 传输方式 | 鉴权 |
|--|--|--|
| **Telegram** | 长轮询 + Webhook | HMAC-SHA256 |
| **Discord** | Gateway WebSocket | Bot token |
| **Slack** | Events API Webhook | HMAC-SHA256 + 重放保护 |
| **钉钉（DingTalk）** | Outgoing Webhook | HMAC-SHA256 |
| **飞书 / Lark** | 事件订阅 | Verification token |
| **企业微信（WeCom）** | AIBot Webhook | SHA1 + AES-256-CBC |
| **通用 Webhook** | HTTP POST | HMAC-SHA256（可选） |
| **CLI** | 交互式 REPL | 仅本地 |

---

## 接入大模型

| Provider | 模型 | 说明 |
|--|--|--|
| **OpenRouter** | 200+ 模型 | 单 Key 接入所有模型 |
| **OpenAI** | GPT-4o, o3, o1, … | 原生 tool-calling |
| **Anthropic** | Claude Sonnet, Opus, … | 原生 tool-calling |
| **Google Gemini** | Gemini Flash, Pro, … | OpenAI 兼容 |
| **Grok（xAI）** | Grok-2, Grok-3, … | OpenAI 兼容 |
| **DeepSeek** | deepseek-chat, deepseek-reasoner, … | 性价比高 |
| **Ollama** | llama3, mistral, qwen, … | 完全本地，无需 API Key |
| **通义千问（Qwen）** | qwen-max, qwen-plus, … | OpenAI 兼容 |
| **Kimi（Moonshot）** | moonshot-v1-*, … | OpenAI 兼容 |
| **智谱 GLM** | glm-4, glm-4-flash, … | OpenAI 兼容 |
| **任意 OpenAI 兼容端点** | — | 自定义 `api_base` |

`ReliabilityChain` 对任意 Provider 序列提供**指数退避 + circuit breaker**：某个 Provider 故障时自动切换到下一个，退避节奏为 1/5/25/60 分钟。

---

## 核心特性

### 🧠 RRF 混合记忆

FTS5 关键词检索 + 本地向量嵌入（FastEmbed，AllMiniLML6v2，384 维，零 API 费用），通过 **Reciprocal Rank Fusion** 融合排名。内置话题切换检测，在每次 recall 前自动剪裁过期上下文。无需外部 Embedding 服务。

### 🔌 MCP 原生支持

[Model Context Protocol](https://modelcontextprotocol.io/) 同时支持 **stdio** 和 **HTTP/SSE** 两种 transport，与 Claude Desktop 配置格式兼容。在 `config.toml` 加入任意 MCP server，其工具立即对所有 Agent 生效，无需编写胶水代码。

### 📨 Message Bus 解耦

`mpsc`（点对点）+ `broadcast`（广播）双总线。Channel 与 Agent 完全独立——接入新渠道或新 Agent，不需要改动任何已有代码。

### ⚡ Provider Circuit Breaker

`ReliabilityChain` 按优先级串联多个 Provider。持续失败后断路器打开（指数退避），随后半开探测恢复。Agent 不会因为单个 Provider 宕机而卡死。

### 🔒 7 层安全体系

```
第 1 层  网络边界    Gateway 默认绑定 127.0.0.1；无 tunnel 时拒绝 0.0.0.0
第 2 层  身份鉴权    配对码 + Bearer token + Webhook HMAC；常量时间比较
第 3 层  用户白名单  每个渠道独立，默认拒绝所有陌生发送者
第 4 层  工具授权    ReadOnly / Supervised / Full 三级自治；支持 Telegram inline 审批
第 5 层  文件系统    工作区隔离 + 符号链接拒绝 + Landlock LSM（Linux）
第 6 层  输出脱敏    26 种正则模式，从每条 LLM 响应中剥离凭据
第 7 层  紧急停止    4 级 estop（KillAll / NetworkKill / DomainBlock / ToolFreeze）+ TOTP
```

此外：请求速率限制、ChaCha20-Poly1305 密钥存储、JSONL 审计日志（可对接 SIEM）默认开启。

### 🏗️ Trait 驱动架构

每个子系统（`Provider`、`Channel`、`Memory`、`Tool`、`Observer`、`Tunnel`）都是 Rust Trait。改一行配置即可切换实现；新增实现无需触碰任何已有代码。各核心 crate——`adaclaw-core`、`adaclaw-providers`、`adaclaw-memory`、`adaclaw-security`、`adaclaw-channels`——独立版本管理，独立测试。

### 🤝 Multi-Agent 委托

```toml
[agents.assistant]
provider = "openrouter"
model    = "anthropic/claude-3.5-sonnet"

[agents.assistant.subagents]
allow = ["coder"]   # assistant 可将编码任务委托给 coder

[agents.coder]
provider    = "anthropic"
model       = "claude-3-5-sonnet-20241022"
temperature = 0.2
tools       = ["shell", "file_read", "file_write"]
```

`DelegateTool` 通过 `tokio::spawn` 异步启动 Sub-Agent，主 Agent 保持响应，多个 Sub-Agent 并行执行。

---

## 配置

最小 `config.toml`（仅 CLI 对话）：

```toml
[providers.openrouter]
api_key = "sk-or-..."

[agents.assistant]
provider = "openrouter"
model    = "anthropic/claude-3.5-sonnet"

[[routing]]
default = true
agent   = "assistant"
```

运行 `adaclaw onboard` 启动交互式配置向导，或复制 [`config.example.toml`](config.example.toml) 查看带完整注释的参考配置。

---

## CLI 命令

```
adaclaw <COMMAND>

Commands:
  run      启动守护进程（渠道 + gateway）
  chat     交互式 CLI 对话
  daemon   管理后台守护进程  start | stop | restart | status
  onboard  首次运行配置向导
  doctor   系统健康检查
  config   显示当前生效配置
  status   查询守护进程状态
  stop     正常停止或触发紧急停止
  help     帮助
```

首次使用建议先运行 `adaclaw doctor`，验证所有子系统就绪。

---

## 参与贡献

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo test --all
cargo clippy -- -D warnings
```

详见 [CONTRIBUTING.md](CONTRIBUTING.md)，包括分支规范、如何新增 Channel、Provider 或 Memory backend。

---

## 开源协议

[Apache License 2.0](LICENSE)

---

<div align="center">
  <sub>用 ⚡ Rust 构建 · 轻量高性能、为可靠性而设计</sub>
</div>
