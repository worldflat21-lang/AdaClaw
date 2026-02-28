# AdaClaw ⚡

<div align="center">
  <p><strong>Lightweight, secure, multi-channel AI Agent Runtime — written in Rust</strong></p>
  <p>
    <a href="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml">
      <img src="https://github.com/worldflat21-lang/AdaClaw/actions/workflows/ci.yml/badge.svg" alt="CI">
    </a>
    <a href="https://github.com/worldflat21-lang/AdaClaw/releases">
      <img src="https://img.shields.io/github/v/release/worldflat21-lang/AdaClaw" alt="Release">
    </a>
    <img src="https://img.shields.io/badge/binary%20size-%3C10MB-brightgreen" alt="Binary size <10MB">
    <img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License">
    <img src="https://img.shields.io/badge/rust-stable-orange" alt="Rust">
  </p>
</div>

---

## 30-Second Quick Start

```bash
# 1. Install (Linux / macOS)
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash

# 2. Configure interactively (sets up provider, channels, workspace)
adaclaw onboard

# 3. Start chatting
adaclaw chat
```

**Windows:**
```powershell
irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
adaclaw onboard
adaclaw chat
```

Or build from source:
```bash
cargo install --git https://github.com/worldflat21-lang/AdaClaw
```

---

## What is AdaClaw?

AdaClaw is an open-source **AI Agent Runtime** — a single binary that connects your LLM of choice to channels like Telegram, Discord, Slack, DingTalk, Feishu, and WeChat Work, with a production-grade security system and hybrid memory.

Unlike cloud-hosted bot platforms, AdaClaw runs **on your own machine or server**. It costs nothing beyond LLM API calls, stores data locally, and never sends your conversations to a third party.

**Key numbers:**
- 📦 Binary size: **< 10 MB** (opt-level = "z", LTO = fat)
- 💾 Memory footprint: **< 5 MB** at idle
- 🚀 Startup time: **< 50 ms**
- 🔒 Security layers: **7** (pairing → allowlist → sandbox → estop → OTP → scrub → audit)

---

## Features

### 🤖 Multi-Provider LLM Support
Connect to any major LLM provider — or multiple at once with automatic failover:

| Provider | Models | Notes |
|----------|--------|-------|
| OpenRouter | 200+ models | Single API key for everything |
| OpenAI | GPT-4o, o1, GPT-4 Turbo | Full native tool-calling |
| Anthropic | Claude 3.5 Sonnet/Opus | Native tool-calling |
| DeepSeek | deepseek-chat, deepseek-reasoner | Affordable alternative |
| Ollama | llama3, mistral, etc. | **No API key** — fully local |

The `ReliabilityChain` provides **exponential backoff + circuit breaker** — if one provider fails, the next one in the chain takes over automatically.

### 💬 Multi-Channel Messaging

Connect to your users wherever they are:

| Channel | Type | Auth |
|---------|------|------|
| Telegram | Long-poll + Webhook | HMAC-SHA256 |
| Discord | Gateway WebSocket | Bot token |
| Slack | Events API Webhook | HMAC-SHA256 + replay protection |
| DingTalk (钉钉) | Outgoing Webhook | HMAC-SHA256 |
| Feishu / Lark (飞书) | Event Subscription | Verification token |
| WeCom / WeChat Work (企业微信) | AIBot Webhook | SHA1 + AES-256-CBC |
| Generic Webhook | HTTP POST | HMAC-SHA256 (optional) |
| CLI | Interactive REPL | Local only |

### 🧠 Advanced Memory System

AdaClaw uses a **hybrid RRF (Reciprocal Rank Fusion)** memory system combining:
- **Vector search** — local FastEmbed (AllMiniLML6V2, 384-dim, zero API cost) or OpenAI embeddings
- **Full-text search** — SQLite FTS5 with BM25 ranking
- **Automatic topic detection** — cosine similarity detects when the user switches topics, pruning irrelevant context

Result: **smarter context injection** than either pure keyword or pure vector search.

### 🔒 7-Layer Security System

```
Layer 1  Network boundary    Gateway binds to 127.0.0.1 by default
Layer 2  Channel auth        Pairing codes + Bearer tokens + Webhook HMAC
Layer 3  User allowlist      Per-channel deny-by-default whitelist
Layer 4  Tool approval       ReadOnly / Supervised / Full autonomy levels
Layer 5  Filesystem          Workspace isolation + symlink detection + Landlock (Linux)
Layer 6  Output scrubbing    26-pattern regex strips credentials from all LLM output
Layer 7  Emergency stop      4-level estop (KillAll/NetworkKill/DomainBlock/ToolFreeze) + TOTP
```

Additional: rate limiting, audit logs (JSONL / SIEM-ready), ChaCha20-Poly1305 secret storage.

### 🤝 Multi-Agent Delegation

Define specialized agents and let them collaborate:

```toml
[agents.assistant]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[agents.assistant.subagents]
allow = ["coder"]          # assistant can delegate coding tasks to coder

[agents.coder]
provider = "anthropic"
model = "claude-3-5-sonnet-20241022"
temperature = 0.2
tools = ["shell", "file_read", "file_write"]
```

The `DelegateTool` spawns sub-agents asynchronously — the main agent stays responsive while the sub-agent works.

---

## Comparison

| Feature | **AdaClaw** | zeroclaw | picoclaw (Go) | nanobot (Python) |
|---------|---------|---------|---------|---------|
| Language | **Rust** | Rust | Go | Python |
| Binary size | **<10 MB** | <9 MB | ~8 MB | N/A |
| Memory (idle) | **<5 MB** | ~12 MB | ~8 MB | ~40 MB |
| 🇨🇳 Chinese channels | **✅ DingTalk/Feishu/WeCom** | partial | partial | ❌ |
| Local embeddings | **✅ FastEmbed** | ✅ | ❌ | ❌ |
| RRF hybrid memory | **✅** | ✅ | ❌ | ❌ |
| Multi-agent routing | **✅ config-driven** | ❌ | ✅ | ❌ |
| Async delegation | **✅** | ❌ | ✅ | ✅ |
| 7-layer security | **✅** | 4 layers | partial | basic |
| Provider failover | **✅ circuit breaker** | ❌ | ✅ | ❌ |
| Message bus | **✅ mpsc+broadcast** | ❌ | ❌ | ✅ |
| Open source | **✅ Apache-2.0** | MIT | MIT | MIT |

---

## Installation

### Pre-compiled Binary (recommended)

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.sh | bash
```

**Windows:**
```powershell
irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
```

**macOS with Homebrew:**
```bash
brew tap worldflat21-lang/adaclaw
brew install adaclaw
```

### Build from Source

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo build --release
# Binary: target/release/adaclaw
```

---

## Configuration

Run the interactive wizard to generate `config.toml`:
```bash
adaclaw onboard
```

Or copy and edit the example:
```bash
cp config.example.toml config.toml
# Edit config.toml with your API keys and settings
```

See [`config.example.toml`](config.example.toml) for a fully annotated reference covering all options.

### Minimal config (CLI chat only)

```toml
[providers.openrouter]
api_key = "sk-or-..."

[agents.assistant]
provider = "openrouter"
model = "anthropic/claude-3.5-sonnet"

[[routing]]
default = true
agent = "assistant"
```

Then: `adaclaw run` or `adaclaw chat`

---

## Docker Deployment (recommended for Full autonomy)

For production use with `autonomy_level = "full"`, always run inside Docker:

```bash
# 1. Configure first
cp config.example.toml config.toml
# Edit config.toml...

# 2. Start
docker compose up -d

# 3. Check logs
docker compose logs -f
```

The included `docker-compose.yml` is hardened: read-only filesystem, dropped capabilities, tmpfs `/tmp`, port bound to `127.0.0.1` only.

---

## CLI Reference

```
adaclaw [COMMAND]

Commands:
  run      Start the daemon (channels + gateway)
  chat     Interactive CLI chat
  daemon   Manage background daemon (start/stop/restart/status)
  onboard  Interactive first-run configuration wizard
  doctor   System health check
  config   Show active configuration
  status   Show daemon status
  stop     Stop daemon / trigger emergency stop
  help     Print help
```

---

## Diagnostics

Run `adaclaw doctor` to check all subsystems:

```
AdaClaw Doctor
==============

✅  config.toml found
✅  Provider 'openrouter' configured with API key
✅  Agent 'assistant' → provider='openrouter' model='anthropic/claude-3.5-sonnet'
✅  Memory: SQLite will be created at 'memory.db' on first use
✅  Gateway: bearer token configured, listening on 127.0.0.1:8080
✅  Security: autonomy_level='supervised' — environment check passed
✅  Binary size: 8.3 MB (target: <10 MB ✓)

─────────────────────────────────────────
Doctor summary: ✅ 7 passed  ⚠️  0 warnings  ❌ 0 failed

✅  All checks passed! AdaClaw is ready to run.
   Run: adaclaw run
```

---

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for a detailed description of the system design, including the message bus, security layers, memory architecture, and module layout.

---

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

Quick start for contributors:
```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo test --all             # run tests
cargo clippy -- -D warnings  # lint
```

---

## License

Licensed under the [Apache License 2.0](LICENSE).

---

<div align="center">
  <sub>Built with ⚡ Rust · Designed for reliability · Open source forever</sub>
</div>
