# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Multi-Channel Architecture**: Support for CLI, Telegram, Discord, Slack, DingTalk, Feishu, and Webhooks.
- **Provider Integrations**: First-class support for OpenAI, Anthropic, Ollama, DeepSeek, and OpenRouter.
- **Memory System**: Reciprocal Rank Fusion (RRF) combining vector embeddings and keyword search (SQLite FTS5).
- **Agent Delegation**: Asynchronous sub-agent spawning for specific tasks.
- **Security Enhancements**: Estop (Emergency Stop) system, rate limiting, PII scrubber, and local workspace sandboxing.
- **Observability**: Prometheus metrics, structured tracing, and audit logs.
- **Tunneling**: Easy localhost exposure via Cloudflare, Tailscale, or Ngrok.
