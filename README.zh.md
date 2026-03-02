<div align="center">
  <img src="assets/Ada103iow103.png" width="80" alt="Ada">
  <h1>AdaClaw ⚡</h1>
  <p><strong>轻量 · 安全 · 多渠道 · 多 Agent 的 Rust AI Agent 运行时</strong></p>
  <p>
    <a href="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml">
      <img src="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="https://github.com/worldflat21-lang/AdaClaw/releases">
      <img src="https://img.shields.io/github/v/release/worldflat21-lang/AdaClaw" alt="Release">
    </a>
    <img src="https://img.shields.io/badge/二进制-%3C10MB-brightgreen" alt="Binary <10MB">
    <img src="https://img.shields.io/badge/内存-%3C5MB-brightgreen" alt="RAM <5MB">
    <img src="https://img.shields.io/badge/协议-Apache--2.0-blue" alt="License">
    <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  </p>
  <p>
    <a href="README.md">English</a>
  </p>
</div>

---

## 产品对比

|  | OpenClaw | NanoBot | PicoClaw | ZeroClaw | **AdaClaw** |
|--|--|--|--|--|--|
| **语言** | TypeScript | Python | Go | Rust | **Rust** |
| **内存占用** | > 1 GB | > 100 MB | < 10 MB | < 5 MB | **< 5 MB** |
| **启动时间** | > 500 s | > 30 s | < 1 s | < 10 ms | **< 50 ms** |
| **多 Agent** | ✅ | ✅ | ✅ | ❌ | ✅ 配置驱动 + 异步委托 |
| **MCP 支持** | ❌ | ✅ stdio + HTTP | ❌ | ❌ | ✅ stdio + HTTP/SSE |
| **RRF 混合记忆** | ❌ | ❌ | ❌ | ✅ | ✅ FTS5 + 向量 + 本地 Embed |
| **安全层数** | DM 配对 | 基础 | workspace 级 | 4 层 | **7 层** |
| **Provider 熔断** | ✅ | ❌ | ✅ | ❌ | ✅ 断路器 |
| **ARM / 树莓派** | ❌ | 部分 | ✅ | ✅ | ✅ |

> 启动时间基准：0.8 GHz 单核边缘设备，AdaClaw 使用 `--release` + opt-level `z` 构建。

---

## 整体架构

```
 渠道接入（Telegram · Discord · Slack · 钉钉 · 飞书 · 企业微信 · Webhook · CLI）
      │
      ▼
 ┌─────────────────────────────────────────────────────┐
 │                    消息总线                          │
 │          mpsc（点对点）+ broadcast（广播）            │
 └────────────┬────────────────────────┬───────────────┘
              │                        │
        ┌─────▼──────┐          ┌──────▼──────┐
        │  Agent     │◄─委托────│  Sub-Agent  │
        │  引擎      │          │  （异步）    │
        └─────┬──────┘          └─────────────┘
              │
   ┌──────────┼──────────────────────┐
   │          │                      │
┌──▼───┐  ┌───▼────┐  ┌─────────────▼──────────┐
│ 记忆 │  │ 工具   │  │    安全层（7 层）        │
│ RRF  │  │ + MCP  │  │配对→白名单→沙箱→紧急停止 │
└──────┘  └────────┘  │→OTP→输出脱敏→审计日志  │
                       └────────────────────────┘
              │
        ┌─────▼──────┐
        │  大模型供应商│ （ReliabilityChain · 断路器）
        │  OpenAI · Anthropic · DeepSeek · Ollama · …
        └────────────┘
```

---

## 快速开始

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
adaclaw onboard   # 交互式向导：配置供应商、渠道、工作区
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

## 安装

### 预编译二进制

| 平台 | 命令 |
|--|--|
| Linux / macOS | `curl -fsSL https://…/install.sh \| bash` |
| Windows（PowerShell） | `irm https://…/install.ps1 \| iex` |
| macOS Homebrew | `brew tap worldflat21-lang/adaclaw && brew install adaclaw` |

发布的二进制覆盖 `x86_64`、`aarch64`（ARM64）、`armv7`，支持 Linux、macOS、树莓派等设备。

### Docker（推荐用于 `autonomy_level = "full"` 场景）

```bash
cp config.example.toml config.toml   # 填写 API Key 等配置
docker compose up -d
docker compose logs -f
```

内置 `docker-compose.yml` 已加固：只读根文件系统、降权、tmpfs `/tmp`、仅监听 `127.0.0.1`。

---

## 渠道接入

| 渠道 | 传输方式 | 鉴权方式 |
|--|--|--|
| **Telegram** | 长轮询 + Webhook | HMAC-SHA256 |
| **Discord** | Gateway WebSocket | Bot Token |
| **Slack** | Events API Webhook | HMAC-SHA256 + 重放保护 |
| **钉钉（DingTalk）** | Outgoing Webhook | HMAC-SHA256 |
| **飞书 / Lark** | 事件订阅 | Verification Token |
| **企业微信（WeCom）** | AIBot Webhook | SHA1 + AES-256-CBC |
| **通用 Webhook** | HTTP POST | HMAC-SHA256（可选） |
| **CLI** | 交互式 REPL | 本地访问 |

---

## 接入的大模型

| 供应商 | 模型 | 备注 |
|--|--|--|
| **OpenRouter** | 200+ 模型 | 单 Key 接入所有模型，推荐首选 |
| **OpenAI** | GPT-4o, o3, o1 | 原生 Tool-calling |
| **Anthropic** | Claude 3.5 / 3.7 Sonnet, Opus | 原生 Tool-calling |
| **Google Gemini** | Gemini 1.5 / 2.0 Flash, Pro | OpenAI 兼容接口 |
| **Grok（xAI）** | Grok-2, Grok-3 | OpenAI 兼容接口 |
| **DeepSeek** | deepseek-chat, deepseek-reasoner | 性价比高 |
| **Ollama** | llama3, mistral, qwen2.5… | 完全本地，无需 API Key |
| **通义千问（Qwen）** | qwen-max, qwen-plus | OpenAI 兼容接口 |
| **Kimi（Moonshot）** | moonshot-v1-* | OpenAI 兼容接口 |
| **智谱 GLM** | glm-4, glm-4-flash | OpenAI 兼容接口 |
| **任意 OpenAI 兼容端点** | — | 自定义 `api_base` |

`ReliabilityChain` 对任意供应商序列提供**指数退避 + 断路器**保护，某个供应商故障时自动切换到下一个。

---

## 特色亮点

### 🧠 RRF 混合记忆

FTS5 关键词检索 + 本地向量嵌入（FastEmbed，AllMiniLML6V2，384 维，零 API 费用）通过 **Reciprocal Rank Fusion** 融合排名。内置话题切换检测，自动剪裁无关历史上下文。无需外部 Embedding API。

这是四个对比项目中最先进的记忆检索方案：OpenClaw 无向量、NanoBot 无本地 Embed、PicoClaw 无向量支持，ZeroClaw 有向量但无本地 Embed。

### 🔌 MCP 原生支持

[Model Context Protocol](https://modelcontextprotocol.io/) 同时支持 **stdio** 和 **HTTP/SSE** 两种 transport，与 Claude Desktop 配置格式兼容。在 `config.toml` 中加入任意 MCP 服务器，其工具立即对所有 Agent 可用。

### 📨 消息总线解耦

`mpsc`（点对点）+ `broadcast`（广播）双总线架构。Channel 与 Agent 完全解耦——新增渠道或 Agent 无需修改任何现有代码。

### ⚡ Provider 熔断器

`ReliabilityChain` 按优先级串联 N 个供应商。连续失败后断路器打开（指数退避），随后半开探测恢复。Agent 永远不会因单个供应商宕机而卡死。

### 🔒 7 层安全体系

```
第 1 层  网络边界    Gateway 默认绑定 127.0.0.1
第 2 层  渠道鉴权    配对码 + Bearer Token + Webhook HMAC
第 3 层  用户白名单  每个渠道独立，默认拒绝所有陌生发送者
第 4 层  工具授权    只读 / 监督 / 完全自主 三级自治模式
第 5 层  文件系统    工作区隔离 + 符号链接检测 + Landlock（Linux）
第 6 层  输出脱敏    26 种正则模式，从所有 LLM 输出中剥离凭据
第 7 层  紧急停止    4 级 estop（KillAll / NetworkKill / DomainBlock / ToolFreeze）+ TOTP
```

附加：请求速率限制、ChaCha20-Poly1305 密钥存储、JSONL 审计日志（SIEM 可对接）。

### 🏗️ Trait 驱动架构

每个子系统——`Provider`、`Channel`、`Memory`、`Tool`、`Observer`、`Tunnel`——都是 Rust Trait。配置文件改一行即可切换实现，新增实现无需改动任何现有代码。各核心 crate（`adaclaw-core`、`adaclaw-providers`、`adaclaw-memory`、`adaclaw-security`、`adaclaw-channels`）独立版本管理。

### 🤝 多 Agent 委托

```toml
[agents.assistant]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[agents.assistant.subagents]
allow = ["coder"]          # 允许 assistant 将编码任务委托给 coder

[agents.coder]
provider = "anthropic"
model = "claude-3-5-sonnet-20241022"
temperature = 0.2
tools = ["shell", "file_read", "file_write"]
```

`DelegateTool` 异步启动子 Agent，主 Agent 无阻塞地继续响应用户。

---

## 配置

最小 `config.toml`（仅 CLI 对话）：

```toml
[providers.openrouter]
api_key = "sk-or-..."

[agents.assistant]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[[routing]]
default = true
agent   = "assistant"
```

运行 `adaclaw onboard` 启动交互式配置向导，或复制 [`config.example.toml`](config.example.toml) 查看完整注释参考。

---

## CLI 命令

```
adaclaw [COMMAND]

命令：
  run      启动守护进程（渠道 + 网关）
  chat     交互式 CLI 对话
  daemon   管理后台守护进程（start / stop / restart / status）
  onboard  首次运行配置向导
  doctor   系统健康检查
  config   显示当前配置
  status   显示守护进程状态
  stop     停止守护进程 / 触发紧急停止
  help     帮助
```

首次启动前建议运行 `adaclaw doctor` 验证所有子系统。

---

## 参与贡献

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo test --all
cargo clippy -- -D warnings
```

详见 [CONTRIBUTING.md](CONTRIBUTING.md)，包括分支规范、如何新增渠道或供应商的说明。

---

## 开源协议

[Apache License 2.0](LICENSE)

---

<div align="center">
  <sub>用 ⚡ Rust 构建 · 轻量、高性能、为可靠性而设计</sub>
</div>
