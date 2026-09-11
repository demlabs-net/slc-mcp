#!/usr/bin/env bash
# SLC MCP — deploy helper (Docker, Obsidian vault, LM Studio provider, web UI).
#
# Usage:
#   ./deploy.sh build     — build the slc-mcp image (MCP + REST + SPA in one process)
#   ./deploy.sh up        — create the vault (+git init), start services, wait for /health
#   ./deploy.sh down      — stop the services
#   ./deploy.sh status    — status + /health (MCP) and /api/health (web UI)
#   ./deploy.sh logs      — follow logs
#   ./deploy.sh migrate   — import KB + seats from legacy Mongo (CLI --from-mongo;
#                           legacy ids are kept unless rename is enabled explicitly)
#   ./deploy.sh reindex   — rebuild embeddings with the current provider
#
# Config: .env in the repo root (copy of .env.example). Vault path: $SLC_VAULT
# (default ~/Obsidian/slc-vault).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

VAULT_PATH="${SLC_VAULT:-$HOME/Obsidian/slc-vault}"
export SLC_VAULT_HOST_PATH="$VAULT_PATH"   # consumed by docker-compose.yml
COMPOSE=(docker compose)
ENV_FILE="$ROOT/.env"
HEALTH_URL="http://127.0.0.1:3000/health"

ensure_env() {
    if [ ! -f "$ENV_FILE" ]; then
        cp "$ROOT/.env.example" "$ENV_FILE"
        echo "created $ENV_FILE from .env.example — review the settings (LMSTUDIO_URL etc.)"
    fi
}

wait_health() {
    local tries=40
    echo "waiting for $HEALTH_URL …"
    for i in $(seq 1 "$tries"); do
        if curl -fsS -m 2 "$HEALTH_URL" >/dev/null 2>&1; then
            echo "healthy."
            return 0
        fi
        sleep 2
    done
    echo "ERROR: server did not respond within $((tries * 2))s — see ./deploy.sh logs" >&2
    return 1
}

cmd_build() {
    "${COMPOSE[@]}" build
}

cmd_up() {
    ensure_env
    mkdir -p "$VAULT_PATH"
    # Vault git repository — required when OBSIDIAN_AUTO_GIT_COMMIT=true.
    if ! git -C "$VAULT_PATH" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git -C "$VAULT_PATH" init -q
        echo "vault git init: $VAULT_PATH"
    fi
    # The server commits with the author identity from env; the committer
    # identity comes from the repo config (otherwise git commit silently fails).
    git -C "$VAULT_PATH" config user.name "slc-mcp" >/dev/null 2>&1 || true
    git -C "$VAULT_PATH" config user.email "slc-mcp@local" >/dev/null 2>&1 || true
    "${COMPOSE[@]}" up -d
    wait_health
    echo
    echo "SLC MCP:   http://127.0.0.1:3000/mcp  (health: $HEALTH_URL)"
    echo "Web UI:    http://127.0.0.1:3000/      (REST /api/*, health: /api/health)"
    echo "vault:     $VAULT_PATH"
}

cmd_down() {
    "${COMPOSE[@]}" down
}

cmd_status() {
    "${COMPOSE[@]}" ps
    echo
    curl -sS -m 3 "$HEALTH_URL" || echo "(health unavailable)"
    echo
    curl -sS -m 3 "http://127.0.0.1:3000/api/health" || echo "(web UI health unavailable)"
}

cmd_logs() {
    "${COMPOSE[@]}" logs -f --tail=50
}

cmd_migrate() {
    ensure_env
    local net
    local mongo_container="${SLC_LEGACY_MONGO_CONTAINER:-slc-mongodb}"
    local mongo_db="${SLC_LEGACY_MONGO_DB:-slc_mcp}"
    local rename="${SLC_MIGRATE_RENAME_WITH_AI:-false}"
    if ! docker inspect "$mongo_container" >/dev/null 2>&1 \
       && docker inspect dev-swarm-slc-mongodb >/dev/null 2>&1; then
        mongo_container=dev-swarm-slc-mongodb
    fi
    net="$(docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$mongo_container" 2>/dev/null | awk '{print $1}')"
    if [ -z "$net" ]; then
        echo "ERROR: legacy Mongo container not found: $mongo_container" >&2
        exit 1
    fi
    local rename_args=()
    case "$rename" in
        true|1|yes|on) rename_args=(--rename-with-ai) ;;
        false|0|no|off) ;;
        *) echo "ERROR: SLC_MIGRATE_RENAME_WITH_AI must be boolean" >&2; exit 1 ;;
    esac
    echo "legacy mongo network: $net"
    echo "target vault:         $VAULT_PATH"
    echo "rename ids with LLM:  $rename"
    docker run --rm \
        --network "$net" \
        --add-host host.docker.internal:host-gateway \
        --env-file "$ENV_FILE" \
        -e OBSIDIAN_AUTO_GIT_COMMIT=false \
        --entrypoint slc-mcp \
        -v "$VAULT_PATH:/data/vault" \
        slc-mcp:local \
        migrate \
            --from-mongo "mongodb://$mongo_container:27017" \
            --db "$mongo_db" \
            --to-vault /data/vault \
            "${rename_args[@]}"
    echo
    echo "Import finished. Restart the server so it rebuilds the index:"
    echo "  ./deploy.sh down && ./deploy.sh up"
    echo "then rebuild embeddings: ./deploy.sh reindex"
}

cmd_reindex() {
    "${COMPOSE[@]}" exec -T slc-mcp slc-mcp reindex-embeddings
}

case "${1:-}" in
    build)   cmd_build ;;
    up)      cmd_up ;;
    down)    cmd_down ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    migrate) cmd_migrate ;;
    reindex) cmd_reindex ;;
    *)
        sed -n '2,14p' "$0"
        exit 1
        ;;
esac
