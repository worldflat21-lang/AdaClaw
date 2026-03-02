# Contributing to AdaClaw

Thank you for your interest in contributing! This guide covers everything you need to get started.

---

## Quick Start

```bash
# 1. Fork and clone
git clone https://github.com/YOUR_USERNAME/AdaClaw.git
cd AdaClaw

# 2. Build
cargo build

# 3. Run tests
cargo test --all

# 4. Lint (must pass before submitting)
cargo clippy --all-targets -- -D warnings

# 5. Format
cargo fmt --all
```

---

## Development Guidelines

### Code Style

- **Rust stable** — no nightly-only features
- `cargo clippy --all-targets -- -D warnings` must produce zero warnings
- `cargo fmt --all` for consistent formatting
- All public items must have doc comments (`///`)
- Modules should have module-level doc comments (`//!`)

### Architecture Principles

Key principles:

1. **Trait-first** — new integrations should implement existing traits (`Provider`, `Channel`, `Memory`, `Tool`)
2. **Data-driven** — new providers/channels register via `ProviderSpec` / config, not hardcoded logic
3. **Security by default** — new features must not bypass the 7-layer security system
4. **Binary size discipline** — avoid pulling in heavy dependencies; check `cargo build --release` size

### Workspace Structure

```
crates/
  adaclaw-core/       # Trait definitions only — minimal deps
  adaclaw-providers/  # LLM provider implementations
  adaclaw-channels/   # Channel implementations
  adaclaw-memory/     # Memory backends
  adaclaw-tools/      # Tool implementations
  adaclaw-security/   # Security modules
  adaclaw-server/     # HTTP gateway (axum)
src/                  # Main binary — wires everything together
```

When adding a new feature, put it in the appropriate crate. The `adaclaw-core` crate must stay minimal (only `async-trait`, `serde`, `anyhow`).

---

## Types of Contributions

### 🐛 Bug Fixes
- Use the [Bug Report](.github/ISSUE_TEMPLATE/bug_report.md) template
- Include a test that reproduces the bug

### ✨ New Channels
Adding a new messaging channel is the most common contribution:

1. Create `crates/adaclaw-channels/src/{channel_name}.rs`
2. Implement the `Channel` trait (see `telegram.rs` as reference)
3. Add construction to `src/daemon/run.rs`
4. Update `config.example.toml` with the new channel config section
5. Add a test (even a basic one for the channel constructor)

### 🤖 New Providers
1. Create `crates/adaclaw-providers/src/{provider_name}.rs`
2. Implement the `Provider` trait
3. Register in `PROVIDER_REGISTRY` in `registry.rs`
4. Add to `config.example.toml`

### 🧠 Memory Backends
1. Create `crates/adaclaw-memory/src/{backend}.rs`
2. Implement the `Memory` trait
3. Register in `factory.rs`

### 🔧 New Tools
1. Create `crates/adaclaw-tools/src/{tool_name}.rs`
2. Implement the `Tool` trait
3. Register in `registry.rs` `all_tools()` function

---

## Testing

```bash
# Run all tests
cargo test --all

# Run tests for a specific crate
cargo test -p adaclaw-memory

# Run a specific test
cargo test test_rrf_fusion_boosts_common

# Run with logging
RUST_LOG=debug cargo test
```

### Writing Tests

- Unit tests go in the same file as the code (`#[cfg(test)]` module)
- Integration tests can go in `tests/` at the crate root
- Use `tempfile::tempdir()` for tests needing filesystem access
- Mock network calls where possible

---

## Pull Request Process

1. **Open an issue first** for significant changes (features, refactors) to discuss the approach
2. **Branch from `main`**: `git checkout -b feature/my-feature`
3. **Write tests** for new functionality
4. **Run the full check suite** before submitting:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   ```
5. **Fill out the PR template** completely
6. **One logical change per PR** — makes review easier

### PR Review Criteria

- CI passes (clippy + tests + format)
- Follows architecture principles
- Has appropriate test coverage
- Documentation updated if needed
- No sensitive data in the code

---

## Reporting Security Issues

**Do not open public GitHub issues for security vulnerabilities.**

Please report security issues by emailing the maintainers directly or using [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability).

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

---

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
