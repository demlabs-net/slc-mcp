# syntax=docker/dockerfile:1
# SLC MCP server — Rust build (candle CPU, no CUDA) on Debian 13 (trixie).
#
# Build:   docker build -t slc-mcp:local .
# Run:     docker compose up -d   (or: docker run -v $HOME/Obsidian/slc-vault:/data/vault -p 127.0.0.1:3000:3000 slc-mcp:local)

# ── builder: Rust toolchain ────────────────────────────────────────────────
FROM rust:1.97-slim-trixie AS builder
WORKDIR /build

# Fetch dependencies first (cache-friendly; Cargo.lock is generated, the
# repo intentionally does not commit it). Dummy sources so manifests parse.
COPY Cargo.toml ./
COPY crates/slc-core/Cargo.toml crates/slc-core/
COPY crates/slc-mcp/Cargo.toml crates/slc-mcp/
RUN mkdir -p crates/slc-core/src crates/slc-mcp/src \
 && touch crates/slc-core/src/lib.rs crates/slc-mcp/src/main.rs \
 && cargo fetch

# Sources. The `librust-inference` submodule is NOT used by this build.
COPY crates/ crates/
RUN cargo build --release -p slc-mcp

# ── runtime: Debian 13 (trixie), no toolchain ─────────────────────────────
FROM debian:trixie-slim

# git — optional vault auto-commit (OBSIDIAN_AUTO_GIT_COMMIT=true);
# curl — healthcheck; ca-certificates — TLS for LLM/embedding endpoints.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --create-home slc \
 && mkdir -p /data/vault \
 && chown -R slc:slc /data/vault

COPY --from=builder /build/target/release/slc-mcp /usr/local/bin/slc-mcp

USER slc
WORKDIR /data/vault

ENV SLC_VAULT_PATH=/data/vault
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3000/health || exit 1

ENTRYPOINT ["slc-mcp", "serve", "--port", "3000"]
