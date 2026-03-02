<div align="center">
  <img src="assets/Ada103iow103.png" width="160" alt="AdaClaw">
  <h1>AdaClaw ⚡</h1>
  <p><strong>Lightweight · Secure · Multi-channel · Multi-Agent AI Agent Runtime in Rust</strong></p>
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
  <p><a href="README.zh.md">中文</a></p>
</div>

AdaClaw is an open-source **AI Agent Runtime** — a single Rust binary that wires your LLM of choice to the channels you already use (Telegram, Discord, Slack, DingTalk, Feishu, WeCom, and more), backed by a hybrid memory engine and a 7-layer security system.

Run it on your own machine, a $10 ARM board, or a container. It costs nothing beyond LLM API calls, keeps all data local, and never phones home.

---

## Comparison

|  | OpenClaw | NanoBot | PicoClaw | ZeroClaw | **AdaClaw** |
|--|--|--|--|--|--|
| **Language** | TypeScript | Python | Go | Rust | **Rust** |
| **RAM** | > 1 GB | > 100 MB | < 10 MB | < 5 MB | **< 5 MB** |
| **Startup** | > 500 s | > 30 s | < 1 s | < 10 ms | **< 50 ms** |
| **Multi-Agent** | ✅ | ✅ | ✅ | ❌ | ✅ config-driven + async delegate |
| **MCP** | ❌ | ✅ stdio + HTTP | ❌ | ❌ | ✅ stdio + HTTP/SSE |
| **RRF Hybrid Memory** | ❌ | ❌ | ❌ | ✅ | ✅ FTS5 + vector + local embed |
| **Security layers** | DM pairing | basic | workspace | 4 | **7** |
| **Provider failover** | ✅ | ❌ | ✅ | ❌ | ✅ circuit breaker |
| **ARM / Raspberry Pi** | ❌ | partial | ✅ | ✅ | ✅ |

> Startup times normalized to 0.8 GHz single-core. AdaClaw built with `--release` + `opt-level = "z"`.

---

## Architecture

```
 Channels  Telegram · Discord · Slack · DingTalk · Feishu · WeCom · WhatsApp · Webhook · CLI
      │
      ▼
 ┌──────────────────────────────────────────────────┐
 │                 Message Bus                      │
 │      mpsc (point-to-point)  +  broadcast         │
 └───────────┬──────────────────────┬───────────────┘
             │                      │
       ┌─────▼──────┐        ┌──────▼──────┐
       │   Agent    │◄─spawn─│  Sub-Agent  │
       │   Engine   │        │   (async)   │
       └─────┬──────┘        └─────────────┘
             │
  ┌──────────┼─────────────────────┐
  │          │                     │
┌─▼────┐  ┌──▼─────┐  ┌───────────▼──────────────┐
│Memory│  │ Tools  │  │  Security  (7 layers)     │
│ RRF  │  │ + MCP  │  │ net→auth→allowlist→sandbox│
└──────┘  └────────┘  │  →estop→scrub→audit       │
                       └──────────────────────────┘
             │
       ┌─────▼──────┐
       │  Providers │  ReliabilityChain · circuit breaker
       │  OpenAI · Anthropic · DeepSeek · Ollama · …
       └────────────┘
```

---

## Quick Start

```bash
# Linux / macOS — one-line install
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
adaclaw onboard   # guided wizard: API key → channels → workspace
adaclaw chat
```

```powershell
# Windows
irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
adaclaw onboard
adaclaw chat
```

Build from source:

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo build --release
./target/release/adaclaw onboard
```

---

## Installation

| Platform | Method |
|--|--|
| Linux / macOS | `curl -fsSL https://…/install.sh \| bash` |
| Windows (PowerShell) | `irm https://…/install.ps1 \| iex` |
| macOS Homebrew | `brew tap worldflat21-lang/adaclaw && brew install adaclaw` |
| Any platform | `cargo install --git https://github.com/worldflat21-lang/AdaClaw` |

Pre-compiled binaries for `x86_64`, `aarch64`, and `armv7` (Linux/macOS) are published on each release.

### Docker

Recommended when running with `autonomy_level = "full"`:

```bash
cp config.example.toml config.toml
# Add your API keys to config.toml
docker compose up -d
docker compose logs -f
```

The bundled `docker-compose.yml` is hardened: read-only rootfs, dropped capabilities, `tmpfs /tmp`, port bound to `127.0.0.1`.

---

## Channels

| Channel | Transport | Auth |
|--|--|--|
| **Telegram** | Long-poll + Webhook | HMAC-SHA256 |
| **Discord** | Gateway WebSocket | Bot token |
| **Slack** | Events API Webhook | HMAC-SHA256 + replay guard |
| **DingTalk** | Outgoing Webhook | HMAC-SHA256 |
| **Feishu / Lark** | Event Subscription | Verification token |
| **WhatsApp** | Cloud API Webhook (HTTPS) | HMAC-SHA256 (X-Hub-Signature-256) |
| **WeCom / WeChat Work** | AIBot Webhook | SHA1 + AES-256-CBC |
| **Generic Webhook** | HTTP POST | HMAC-SHA256 (optional) |
| **CLI** | Interactive REPL | Local only |

---

## LLM Providers

| Provider | Models | Notes |
|--|--|--|
| **OpenRouter** | 200+ models | Single key for all models |
| **OpenAI** | GPT-4o, o3, o1, … | Native tool-calling |
| **Anthropic** | Claude Sonnet, Opus, … | Native tool-calling |
| **Google Gemini** | Gemini Flash, Pro, … | OpenAI-compat |
| **Grok (xAI)** | Grok-2, Grok-3, … | OpenAI-compat |
| **DeepSeek** | deepseek-chat, deepseek-reasoner, … | Cost-efficient |
| **Ollama** | llama3, mistral, qwen, … | Fully local — no API key |
| **Qwen (Alibaba)** | qwen-max, qwen-plus, … | OpenAI-compat |
| **Kimi (Moonshot)** | moonshot-v1-*, … | OpenAI-compat |
| **GLM (Zhipu)** | glm-4, glm-4-flash, … | OpenAI-compat |
| **Any OpenAI-compat** | — | Custom `api_base` |

`ReliabilityChain` wraps any provider sequence with **exponential backoff + circuit breaker**. If one degrades, the next takes over automatically.

---

## Highlights

### 🧠 RRF Hybrid Memory

FTS5 keyword search + local vector embeddings (FastEmbed, AllMiniLML6v2, 384-dim, zero API cost) merged via **Reciprocal Rank Fusion**. Automatic topic-shift detection prunes stale context before each recall. No external embedding service needed.

### 🔌 Native MCP Support

[Model Context Protocol](https://modelcontextprotocol.io/) over both **stdio** and **HTTP/SSE** transports, Claude Desktop config-compatible. Any MCP server added to `config.toml` becomes a first-class tool for every agent — no glue code required.

### 📨 Message Bus Decoupling

`mpsc` point-to-point + `broadcast` dual bus. Channels and Agents are fully decoupled — adding a new integration requires zero changes to existing code.

### ⚡ Provider Circuit Breaker

`ReliabilityChain` sequences N providers by priority. On sustained failure the breaker opens (exponential backoff: 1/5/25/60 min), then half-opens to probe recovery. Agents never stall on a dead provider.

### 🔒 7-Layer Security

```
Layer 1  Network       Gateway binds 127.0.0.1 by default; refuses 0.0.0.0 without tunnel
Layer 2  Auth          Pairing codes + Bearer tokens + Webhook HMAC; constant-time compare
Layer 3  Allowlist     Per-channel deny-by-default sender whitelist
Layer 4  Approval      ReadOnly / Supervised / Full autonomy; inline approval via Telegram
Layer 5  Filesystem    Workspace isolation + symlink rejection + Landlock LSM (Linux)
Layer 6  Scrubbing     26-pattern regex strips credentials from every LLM response
Layer 7  Emergency     4-level estop (KillAll / NetworkKill / DomainBlock / ToolFreeze) + TOTP
```

Rate limiting, ChaCha20-Poly1305 secret storage, and JSONL audit log (SIEM-ready) are always on.

### 🏗️ Trait-Driven Architecture

Every subsystem (`Provider`, `Channel`, `Memory`, `Tool`, `Observer`, `Tunnel`) is a Rust trait. Swap an implementation with one config line. Add a new one without touching existing code. Each crate — `adaclaw-core`, `adaclaw-providers`, `adaclaw-memory`, `adaclaw-security`, `adaclaw-channels` — is independently versioned and testable.

### 🤝 Multi-Agent Delegation

```toml
[agents.assistant]
provider = "openrouter"
model    = "anthropic/claude-3.5-sonnet"

[agents.assistant.subagents]
allow = ["coder"]

[agents.coder]
provider    = "anthropic"
model       = "claude-3-5-sonnet-20241022"
temperature = 0.2
tools       = ["shell", "file_read", "file_write"]
```

`DelegateTool` spawns sub-agents asynchronously via `tokio::spawn`. The orchestrating agent stays responsive while workers run in parallel.

---

## Configuration

Minimal `config.toml` for CLI chat:

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

Run `adaclaw onboard` for the guided wizard, or copy [`config.example.toml`](config.example.toml) for the full annotated reference.

---

## CLI Reference

```
adaclaw <COMMAND>

Commands:
  run      Start daemon (channels + gateway)
  chat     Interactive CLI chat
  daemon   Manage background daemon  start | stop | restart | status
  onboard  First-run configuration wizard
  doctor   System health check
  config   Show active configuration
  status   Show daemon status
  stop     Graceful stop or emergency stop
  help     Print help
```

---

## Contributing

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo test --all
cargo clippy -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch conventions and how to add a new channel, provider, or memory backend.

---

## License

[Apache License 2.0](LICENSE)

---

<div align="center">
  <sub>Built with ⚡ Rust · Lightweight & High Performance · Designed for reliability</sub>
</div>
