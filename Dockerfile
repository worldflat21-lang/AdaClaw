# AdaClaw — Multi-stage Dockerfile
#
# Stage 1: builder  — compile with release optimizations
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

# Cache dependency compilation separately from source changes
# Copy manifests first
COPY Cargo.toml Cargo.lock ./
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
