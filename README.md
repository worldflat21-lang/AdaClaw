# AdaClaw

<div align="center">
  <img src="assets/logo.png" alt="AdaClaw Logo" width="200" />
  <p>Lightweight, secure, multi-channel Rust AI Agent Runtime</p>
</div>

## Features

- **Multi-Channel Support**: Native integrations for Telegram, Discord, Slack, DingTalk, Feishu, WeCom, and CLI.
- **Provider Agnostic**: Works with OpenAI, Anthropic, Ollama, DeepSeek, OpenRouter, and more.
- **Advanced Memory**: Built-in SQLite + FTS5 memory system with RRF (Reciprocal Rank Fusion) and local vector embeddings (FastEmbed).
- **Security First**: 7-layer defense system including container detection, RBAC approvals, scrubbers, and Estop.
- **High Performance**: Built in Rust for speed, low memory footprint, and reliable concurrency.
- **Agent Delegation**: Multi-agent system where agents can spawn asynchronous sub-agents to handle specific tasks.

## Quick Start

### 1. Installation

You can download the latest pre-compiled binary from the [Releases](https://github.com/worldflat21-lang/AdaClaw/releases) page, or build it yourself using Cargo:

```bash
git clone https://github.com/worldflat21-lang/AdaClaw.git
cd AdaClaw
cargo build --release
```

### 2. Initialization

Run the onboard command to generate a default configuration:

```bash
adaclaw onboard
```

Follow the interactive prompts to set up your LLM provider and API keys.

### 3. Run the CLI Agent

You can immediately start chatting in the terminal:

```bash
adaclaw chat
```

### 4. Run as a Daemon (Background Service)

To run AdaClaw as a background service connecting to your configured channels (e.g., Telegram, Discord):

```bash
adaclaw daemon start
```

## Docker Deployment

We provide a secure, read-only Docker Compose setup:

```bash
# Edit config.toml first
cp config.example.toml config.toml

# Start the container
docker compose up -d
```

## Configuration

AdaClaw is configured via `config.toml`. See `config.example.toml` for a complete example covering all options.

## License

This project is licensed under the MIT OR Apache-2.0 License. See the [LICENSE](LICENSE) file for details.
