# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Planned
- Web UI (AdaClaw Dashboard)
- PostgreSQL memory backend for distributed deployments
- WebRTC voice channel with Whisper ASR + TTS

---

## [0.1.0] - 2026-03-01

Initial open-source release of AdaClaw — Lightweight, Secure, Multi-Channel Rust AI Agent Runtime.

### Added

#### Core Architecture
- **Workspace crate structure**: `adaclaw-core` / `adaclaw-providers` / `adaclaw-memory` / `adaclaw-tools` / `adaclaw-channels` / `adaclaw-security` / `adaclaw-server` — all Trait-based, fully pluggable
- **Message Bus**: `tokio::sync::mpsc` inbound + `broadcast` outbound, decoupling channels from agents
- **AgentEngine**: Tool-call loop with multi-format parser (OpenAI JSON / XML / Markdown fence / GLM), dedup, parallel execution, context-window auto-recovery
- **History compaction**: "Congee" rolling-summary strategy with LLM summarization + hard-trim fallback
- **Multi-Agent routing**: Config-driven 3-priority routing (channel_pattern > sender_id > default)
- **Async Agent delegation**: `DelegateTool` spawns sub-agents via `tokio::spawn`, result fed back through `channel="system"` bypass

#### LLM Providers
- **OpenAI** (+ all OpenAI-compatible endpoints)
- **Anthropic Claude**
- **OpenRouter** (200+ models via single API key)
- **DeepSeek** (deepseek-chat / deepseek-reasoner)
- **Ollama** (fully local, no API key required)
- **Groq** (LLM + Whisper voice transcription)
- **ReliabilityChain**: Exponential backoff + circuit breaker with exponential cooldown (1/5/25/60 min), error classification (RateLimit/Auth/BadRequest/Billing/ServerError/Timeout)

#### Memory System
- **SQLite + FTS5** full-text search with BM25 ranking
- **sqlite-vec** vector search (feature-gated)
- **RRF (Reciprocal Rank Fusion)** hybrid retrieval (k=60)
- **FastEmbed** local embeddings (AllMiniLML6V2, 384-dim, zero API cost)
- **OpenAI embeddings** (text-embedding-3-small)
- **Topic detection**: Automatic topic switching with context pruning (RecallScope: Full/FactsOnly/CurrentTopic/Clean)
- **QMD query decomposition**: Complex queries split into 2-5 sub-queries, N-way RRF merge
- **Memory consolidation**: Two-phase LLM-based dedup/merge, cron-schedulable
- **GlobalMemory** wrapper for shared read-only knowledge base

#### Channels
- **Telegram**: Long-polling + webhook, HMAC-SHA256, typing loop, mention-only mode, /start /help commands, 409 conflict recovery
- **Discord**: Gateway WebSocket, HEARTBEAT, typing loop, exponential backoff reconnect
- **Slack**: Events API webhook, HMAC-SHA256 + replay protection, mrkdwn formatting, thread reply support
- **DingTalk** (钉钉): HMAC-SHA256 webhook
- **Feishu/Lark** (飞书): Event subscription, tenant_access_token auto-refresh, non-text message handling
- **WeChat Work/WeCom** (企业微信): SHA1 verification, AES-256-CBC decryption (non-standard block_size=32 PKCS7)
- **WhatsApp Business Cloud API**: Meta webhook, X-Hub-Signature-256 HMAC, constant-time comparison
- **Generic Webhook**: HMAC-SHA256 optional
- **CLI**: Interactive REPL

#### Security (7-Layer Defence)
- **Network boundary**: Gateway binds `127.0.0.1` by default
- **Bearer Token auth**: Gateway `POST /v1/chat` + `POST /v1/stop` protected; constant-time comparison
- **Pairing codes**: `GET /pair` → cryptographically secure `OsRng` 6-digit one-time codes (10 min TTL)
- **User allowlist**: Per-channel deny-by-default whitelist
- **Tool approval**: `AutonomyLevel` (ReadOnly/Supervised/Full), Telegram inline keyboard ✅/❌, session allowlist, `always_ask`/`auto_approve` lists, pending request expiry
- **Workspace isolation**: Path traversal detection, symlink rejection, system directory blacklist, Linux Landlock LSM
- **Credential scrubbing**: 26-pattern regex, 3-pass (Bearer → URL → KV), Unicode-safe
- **Emergency Stop**: 4 levels (KillAll/NetworkKill/DomainBlock/ToolFreeze), disk persistence, TOTP verification option
- **Rate limiting**: per_user/per_channel sliding window, daily cost budget, max_actions_per_hour
- **Audit log**: Structured JSONL (SIEM-ready)
- **Secret storage**: ChaCha20-Poly1305 encrypted

#### Observability
- **Prometheus metrics** (8 metric families, pure atomic, no external exporter process)
- **Structured tracing** via `tracing` + `tracing-subscriber`
- **Runtime tracer**: Rolling JSONL event log
- **Audit log**: JSONL with `AuditKind` enum (20+ event types)

#### CLI Commands
- `adaclaw run` — Start daemon (gateway + channels + agent loop)
- `adaclaw chat` — Interactive CLI REPL
- `adaclaw onboard` — Interactive first-run wizard
- `adaclaw doctor` — Full system health check
- `adaclaw stop` — Send stop signal to running daemon (HTTP)
- `adaclaw status` — Query daemon status (HTTP)
- `adaclaw config check` — Validate config.toml with semantic field-level errors
- `adaclaw skill list/install/remove/audit` — ClawHub skill management

#### Ecosystem
- **MCP client**: Full Model Context Protocol support (stdio + HTTP transport), Claude Desktop config format compatible
- **Heartbeat scheduler**: `HEARTBEAT.md` task list, configurable interval, MessageBus injection
- **Tunnel integration**: Cloudflare / ngrok / Tailscale (process-level, Drop-based cleanup)
- **Config versioning + migration**: `config_version` field, forward-only migration, 28 semantic validation rules

#### Infrastructure
- Release profile: `opt-level="z"`, `lto="fat"`, `strip`, `panic="abort"` — target binary < 10 MB
- Docker: hardened `docker-compose.yml` (`read_only`, `tmpfs`, `cap_drop=ALL`, `no-new-privileges`)
- Multi-stage `Dockerfile` for reproducible builds
- CI: clippy (`-D warnings`) + tests + binary size check + `cargo fmt`
- Release: cross-platform matrix (Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64) with SHA256 checksums
- Security CI: `cargo audit` + `cargo-deny` (weekly + PR trigger)
- Install scripts: `install.sh` (Linux/macOS, architecture detection + SHA256 verification) + `install.ps1` (Windows)
- Homebrew formula
