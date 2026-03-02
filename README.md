<div align="center">
  <img src="assets/Ada103iow103.png" width="80" alt="Ada">
  <h1>AdaClaw ⚡</h1>
  <p><strong>Lightweight · Secure · Multi-channel · Multi-Agent AI Agent Runtime — written in Rust</strong></p>
  <p>
    <a href="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml">
      <img src="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="https://github.com/worldflat21-lang/AdaClaw/releases">
      <img src="https://img.shields.io/github/v/release/worldflat21-lang/AdaClaw" alt="Release">
    </a>
    <img src="https://img.shields.io/badge/binary-%3C10MB-brightgreen" alt="Binary <10MB">
    <img src="https://img.shields.io/badge/RAM-%3C5MB-brightgreen" alt="RAM <5MB">
    <img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License">
    <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  </p>
  <p>
    <a href="README.zh.md">中文文档</a>
  </p>
</div>

---

## Comparison

|  | OpenClaw | NanoBot | PicoClaw | ZeroClaw | **AdaClaw** |
|--|--|--|--|--|--|
| **Language** | TypeScript | Python | Go | Rust | **Rust** |
| **RAM** | > 1 GB | > 100 MB | < 10 MB | < 5 MB | **< 5 MB** |
| **Startup** | > 500 s | > 30 s | < 1 s | < 10 ms | **< 50 ms** |
| **Multi-Agent** | ✅ | ✅ | ✅ | ❌ | ✅ config + async delegate |
| **MCP** | ❌ | ✅ stdio + HTTP | ❌ | ❌ | ✅ stdio + HTTP/SSE |
| **RRF Hybrid Memory** | ❌ | ❌ | ❌ | ✅ | ✅ FTS5 + vector + local embed |
| **Security layers** | DM pairing | basic | workspace | 4 | **7** |
| **Provider failover** | ✅ | ❌ | ✅ | ❌ | ✅ circuit breaker |
| **ARM / Raspberry Pi** | ❌ | partial | ✅ | ✅ | ✅ |

> Startup times normalized to a 0.8 GHz single-core edge board. AdaClaw release builds measured with `--release` + opt-level `z`.

---

## Architecture

```
 Channels (Telegram · Discord · Slack · DingTalk · Feishu · WeCom · Webhook · CLI)
      │
      ▼
 ┌─────────────────────────────────────────────────────┐
 │                   Message Bus                       │
 │         mpsc (point-to-point) + broadcast           │
 └────────────┬────────────────────────┬───────────────┘
              │                        │
        ┌─────▼──────┐          ┌──────▼──────┐
        │  Agent     │◄─delegate│  Sub-Agent  │
        │  Engine    │          │  (async)    │
        └─────┬──────┘          └─────────────┘
              │
   ┌──────────┼──────────────────────┐
   │          │                      │
┌──▼───┐  ┌───▼────┐  ┌─────────────▼──────────┐
│Memory│  │Tools   │  │Security Layer (7 layers)│
│ RRF  │  │+ MCP   │  │pairing→allowlist→sandbox│
└──────┘  └────────┘  │→estop→OTP→scrub→audit  │
                       └────────────────────────┘
              │
        ┌─────▼──────┐
        │  Providers │  (ReliabilityChain · circuit breaker)
        │  OpenAI · Anthropic · DeepSeek · Ollama · …
        └────────────┘
```

---

## Quick Start

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
adaclaw onboard   # interactive wizard: provider, channels, workspace
adaclaw chat
```

```powershell
# Windows
irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
adaclaw onboard
adaclaw chat
```

Or build from source:

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo build --release
./target/release/adaclaw onboard
```

---

## Installation

### Pre-compiled binary

| Platform | Command |
|--|--|
| Linux / macOS | `curl -fsSL https://…/install.sh \| bash` |
| Windows (PowerShell) | `irm https://…/install.ps1 \| iex` |
| macOS Homebrew | `brew tap worldflat21-lang/adaclaw && brew install adaclaw` |

Binaries are published for `x86_64`, `aarch64` (ARM64), and `armv7` on Linux and macOS.

### Docker (recommended for `autonomy_level = "full"`)

```bash
cp config.example.toml config.toml   # edit with your API keys
docker compose up -d
docker compose logs -f
```

The bundled `docker-compose.yml` is hardened: read-only rootfs, dropped capabilities, tmpfs `/tmp`, port bound to `127.0.0.1` only.

---

## Channels

| Channel | Transport | Auth |
|--|--|--|
| **Telegram** | Long-poll + Webhook | HMAC-SHA256 |
| **Discord** | Gateway WebSocket | Bot token |
| **Slack** | Events API Webhook | HMAC-SHA256 + replay guard |
| **DingTalk** | Outgoing Webhook | HMAC-SHA256 |
| **Feishu / Lark** | Event Subscription | Verification token |
| **WeCom / WeChat Work** | AIBot Webhook | SHA1 + AES-256-CBC |
| **Generic Webhook** | HTTP POST | HMAC-SHA256 (optional) |
| **CLI** | Interactive REPL | Local only |

---

## LLM Providers

| Provider | Models | Notes |
|--|--|--|
| **OpenRouter** | 200+ models | Single key for all models — recommended |
| **OpenAI** | GPT-4o, o3, o1 | Native tool-calling |
| **Anthropic** | Claude 3.5 / 3.7 Sonnet, Opus | Native tool-calling |
| **Google Gemini** | Gemini 1.5 / 2.0 Flash, Pro | OpenAI-compat |
| **Grok (xAI)** | Grok-2, Grok-3 | OpenAI-compat |
| **DeepSeek** | deepseek-chat, deepseek-reasoner | Cost-efficient |
| **Ollama** | llama3, mistral, qwen2.5… | Fully local — no API key |
| **Qwen (Alibaba)** | qwen-max, qwen-plus | OpenAI-compat |
| **Kimi (Moonshot)** | moonshot-v1-* | OpenAI-compat |
| **GLM (Zhipu)** | glm-4, glm-4-flash | OpenAI-compat |
| **Any OpenAI-compat** | — | Custom `api_base` |

The `ReliabilityChain` wraps any sequence of providers with **exponential backoff + circuit breaker** — automatic failover if a provider is degraded.

---

## Highlights

### 🧠 RRF Hybrid Memory

FTS5 keyword search + local vector embeddings (FastEmbed, AllMiniLML6V2, 384-dim, zero API cost) fused with **Reciprocal Rank Fusion**. Automatic topic detection prunes irrelevant history when the conversation shifts. No external embedding API required.

### 🔌 Native MCP Support

[Model Context Protocol](https://modelcontextprotocol.io/) over **stdio** and **HTTP/SSE** — both transports, Claude Desktop config-compatible. Drop any MCP server into `config.toml` and its tools are available to every agent automatically.

### 📨 Message Bus Decoupling

`mpsc` (point-to-point) + `broadcast` dual bus. Channels and Agents are fully independent — adding a new channel or agent requires zero changes to existing code.

### ⚡ Provider Circuit Breaker

`ReliabilityChain` wraps N providers in priority order. On repeated failure it opens the breaker (exponential backoff), then half-opens to probe recovery. The agent never stalls waiting for a dead provider.

### 🔒 7-Layer Security

```
Layer 1  Network       Gateway binds 127.0.0.1 by default
Layer 2  Auth          Pairing codes + Bearer tokens + Webhook HMAC
Layer 3  Allowlist     Per-channel deny-by-default sender whitelist
Layer 4  Approval      ReadOnly / Supervised / Full autonomy levels
Layer 5  Filesystem    Workspace isolation + symlink detection + Landlock (Linux)
Layer 6  Scrubbing     26-pattern regex strips credentials from all LLM output
Layer 7  Emergency     4-level estop (KillAll / NetworkKill / DomainBlock / ToolFreeze) + TOTP
```

Additional: rate limiting, ChaCha20-Poly1305 secret storage, JSONL audit log (SIEM-ready).

### 🏗️ Trait-Driven Architecture

Every subsystem — `Provider`, `Channel`, `Memory`, `Tool`, `Observer`, `Tunnel` — is a Rust trait. Swap implementations with a config change; add a new one without touching existing code. The core crates (`adaclaw-core`, `adaclaw-providers`, `adaclaw-memory`, `adaclaw-security`, `adaclaw-channels`) are independently versioned.

### 🤝 Multi-Agent Delegation

```toml
[agents.assistant]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[agents.assistant.subagents]
allow = ["coder"]

[agents.coder]
provider = "anthropic"
model = "claude-3-5-sonnet-20241022"
temperature = 0.2
tools = ["shell", "file_read", "file_write"]
```

`DelegateTool` spawns sub-agents asynchronously — the main agent remains responsive while sub-agents run in parallel.

---

## Configuration

Minimal `config.toml` (CLI chat only):

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

Run `adaclaw onboard` for the interactive wizard, or copy [`config.example.toml`](config.example.toml) for a fully annotated reference.

---

## CLI Reference

```
adaclaw [COMMAND]

Commands:
  run      Start daemon (channels + gateway)
  chat     Interactive CLI chat
  daemon   Manage background daemon (start / stop / restart / status)
  onboard  First-run configuration wizard
  doctor   System health check
  config   Show active configuration
  status   Show daemon status
  stop     Stop daemon / trigger emergency stop
  help     Print help
```

Run `adaclaw doctor` to verify all subsystems before the first start.

---

## Contributing

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo test --all
cargo clippy -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines, branch conventions, and how to add a new channel or provider.

---

## License

Licensed under the [Apache License 2.0](LICENSE).

---

<div align="center">
  <sub>Built with ⚡ Rust · Lightweight & High Performance · Designed for reliability</sub>
</div>
