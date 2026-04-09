# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.85-slim-bookworm AS builder

# Install system deps for native-tls / openssl
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies by copying manifests first
COPY Cargo.toml Cargo.lock* ./

# Create dummy main so cargo can fetch/compile deps
RUN mkdir -p src && echo 'fn main(){}' > src/main.rs
RUN cargo build --release 2>/dev/null || true

# Copy real source and rebuild
COPY src ./src
RUN touch src/main.rs
RUN cargo build --release

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/mexc-sniper /app/mexc-sniper

# Health check: just ensure process is alive
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD pgrep -x mexc-sniper || exit 1

ENTRYPOINT ["/app/mexc-sniper"]
