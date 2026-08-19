# syntax=docker/dockerfile:1
# SLC MCP server — Rust build (candle CPU, no CUDA) on Debian 13 (trixie).
# Один процесс: MCP (JSON-RPC /mcp, SSE /sse, /messages) + REST /api/* +
# SPA-статика веб-морды (web-ui/dist) — владелец vault один, конфликтов нет.
#
# Build:   docker build -t slc-mcp:local .
# Run:     docker compose up -d   (or: docker run -v $HOME/Obsidian/slc-vault:/data/vault -p 127.0.0.1:3000:3000 slc-mcp:local)

# ── frontend: SPA build (web-ui/dist) ───────────────────────────────────────
FROM node:22-alpine AS frontend
WORKDIR /app
COPY web-ui/package.json web-ui/package-lock.json ./
RUN npm ci
COPY web-ui/ web-ui/
WORKDIR /app/web-ui
RUN npm run build

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

# git — optional vault auto-commit (OBSIDIAN_AUTO_GIT_COMMIT=true) + push
# по ssh (openssh-client); curl — healthcheck; ca-certificates — TLS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git curl openssh-client \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --create-home slc \
 && mkdir -p /data/vault /app/dist \
 && chown -R slc:slc /data/vault /app/dist

COPY --from=builder /build/target/release/slc-mcp /usr/local/bin/slc-mcp
COPY --from=frontend /app/web-ui/dist /app/dist

USER slc
WORKDIR /data/vault

ENV SLC_VAULT_PATH=/data/vault \
    SLC_WEBUI_DIST=/app/dist
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3000/health || exit 1

ENTRYPOINT ["slc-mcp", "serve", "--port", "3000"]
