# AdaClaw — Multi-stage Dockerfile
#
# Stage 1: builder  — compile with release optimizations (two-step dep caching)
# Stage 2: runtime  — minimal debian:bookworm-slim image
#
# Build:
#   docker build -t adaclaw:latest .
#
# Run (with docker-compose):
#   docker compose up -d
#
# Or directly:
#   docker run -it --rm \
#     -v ./config.toml:/app/config.toml:ro \
#     -v ./workspace:/app/workspace \
#     -p 127.0.0.1:8080:8080 \
#     adaclaw:latest

# ── Stage 1: Builder ──────────────────────────────────────────────────────────

FROM rust:1.82-bookworm AS builder

WORKDIR /build

# ── Step A: Dependency caching ─────────────────────────────────────────────────
#
# Copy only the workspace manifest, lock file, and per-crate Cargo.toml files.
# Source code (src/ and crates/*/src/) is intentionally excluded.
#
# This layer is invalidated only when dependency versions change (Cargo.toml /
# Cargo.lock edits).  Editing application source code does NOT bust this layer,
# so `cargo build` can reuse the pre-compiled external dependency artifacts.
COPY Cargo.toml Cargo.lock ./
COPY crates/adaclaw-core/Cargo.toml       crates/adaclaw-core/Cargo.toml
COPY crates/adaclaw-channels/Cargo.toml   crates/adaclaw-channels/Cargo.toml
COPY crates/adaclaw-memory/Cargo.toml     crates/adaclaw-memory/Cargo.toml
COPY crates/adaclaw-providers/Cargo.toml  crates/adaclaw-providers/Cargo.toml
COPY crates/adaclaw-tools/Cargo.toml      crates/adaclaw-tools/Cargo.toml
COPY crates/adaclaw-security/Cargo.toml   crates/adaclaw-security/Cargo.toml
COPY crates/adaclaw-server/Cargo.toml     crates/adaclaw-server/Cargo.toml

# Create minimal stub source files.  `cargo build` needs them to resolve the
# workspace graph and compile all external dependencies.  The stubs may fail to
# compile (empty lib.rs lacks the real symbols), but by that point all external
# crates are already compiled and cached in the layer.
RUN set -eux; \
    mkdir -p src && echo 'fn main() {}' > src/main.rs; \
    for crate in adaclaw-core adaclaw-channels adaclaw-memory \
                 adaclaw-providers adaclaw-tools adaclaw-security adaclaw-server; do \
        mkdir -p "crates/$crate/src" && touch "crates/$crate/src/lib.rs"; \
    done; \
    cargo build --release --bin adaclaw 2>&1 || true; \
    rm -rf src crates/*/src

# ── Step B: Final build with real source ──────────────────────────────────────
#
# Cargo reuses the pre-compiled external-dependency artifacts from Step A.
# Only the workspace crates themselves are (re)compiled from the real source.
# Any change to src/ or crates/*/src/ only re-runs this step.
COPY crates/ crates/
COPY src/ src/

# Build in release mode with the same profile as the CI binary-size check
RUN cargo build --release --bin adaclaw

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────

FROM debian:bookworm-slim AS runtime

# Install only the minimal runtime dependencies
# - ca-certificates: HTTPS connections to LLM providers
# - libssl3: TLS (reqwest)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user for the daemon
RUN useradd -m -u 1000 -s /bin/sh adaclaw

WORKDIR /app

# Copy only the compiled binary
COPY --from=builder /build/target/release/adaclaw /app/adaclaw

# Default directory structure expected by the daemon
RUN mkdir -p /app/workspace /app/.adaclaw \
    && chown -R adaclaw:adaclaw /app

USER adaclaw

# Expose the gateway port (bind is configured in config.toml)
EXPOSE 8080

# Health check via the /v1/status endpoint
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD /app/adaclaw status 2>/dev/null || exit 1

# Default command: start the daemon
# Mount config.toml via -v ./config.toml:/app/config.toml:ro
CMD ["/app/adaclaw", "run"]
