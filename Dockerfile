# syntax=docker/dockerfile:1
# SLC MCP server — бинарь собирается локально (`cargo deb -p slc-mcp -o dist`)
# и доставляется .deb-пакетом; SPA (web-ui/dist) собирается локально и копируется
# предсобранным. Внутри Docker компиляции нет. Хост и образ — Debian 13 (trixie).
FROM debian:trixie-slim

# git — авто-коммит волта (OBSIDIAN_AUTO_GIT_COMMIT=true) + push по ssh;
# curl — healthcheck; ca-certificates — TLS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git curl openssh-client \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --create-home slc \
 && mkdir -p /data/vault /app/dist /home/slc/.ssh \
 && chmod 0700 /home/slc/.ssh \
 && chown -R slc:slc /data/vault /app/dist /home/slc/.ssh

COPY dist/slc-mcp_*.deb /tmp/slc-mcp.deb
RUN dpkg -i /tmp/slc-mcp.deb && rm -f /tmp/slc-mcp.deb

USER slc
WORKDIR /data/vault

ENV SLC_VAULT_PATH=/data/vault \
    SLC_WEBUI_DIST=/usr/share/slc-mcp/webui
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3000/health || exit 1

ENTRYPOINT ["/usr/local/bin/slc-mcp"]
CMD ["serve"]
