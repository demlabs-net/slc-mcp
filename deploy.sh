#!/usr/bin/env bash
# SLC MCP — deploy helper (Docker, Obsidian vault, LM Studio provider, web UI).
#
# Usage:
#   ./deploy.sh build     — собрать образ slc-mcp (MCP + REST + SPA одним процессом)
#   ./deploy.sh up        — создать vault (+git init), поднять сервис, ждать /health
#   ./deploy.sh down      — остановить сервисы
#   ./deploy.sh status    — статус + /health (MCP) и /api/health (webui)
#   ./deploy.sh logs      — логи (follow)
#   ./deploy.sh migrate   — импорт БЗ+сидов из легаси-Mongo (CLI --from-mongo,
#                           с AI-переименованием id через LLM из .env)
#   ./deploy.sh reindex   — пересобрать эмбеддинги текущим провайдером
#
# Config: .env в корне репо (копия .env.example). Путь vault: $SLC_VAULT
# (по умолчанию ~/Obsidian/slc-vault).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

VAULT_PATH="${SLC_VAULT:-$HOME/Obsidian/slc-vault}"
export SLC_VAULT_HOST_PATH="$VAULT_PATH"   # используется docker-compose.yml
COMPOSE=(docker compose)
ENV_FILE="$ROOT/.env"
HEALTH_URL="http://127.0.0.1:3000/health"

ensure_env() {
    if [ ! -f "$ENV_FILE" ]; then
        cp "$ROOT/.env.example" "$ENV_FILE"
        echo "created $ENV_FILE from .env.example — проверьте настройки (LMSTUDIO_URL и т.п.)"
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
    echo "ERROR: сервер не ответил за $((tries * 2))с — смотри ./deploy.sh logs" >&2
    return 1
}

cmd_build() {
    "${COMPOSE[@]}" build
}

cmd_up() {
    ensure_env
    mkdir -p "$VAULT_PATH"
    # git-репозиторий vault — нужен при OBSIDIAN_AUTO_GIT_COMMIT=true.
    if ! git -C "$VAULT_PATH" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git -C "$VAULT_PATH" init -q
        echo "vault git init: $VAULT_PATH"
    fi
    # Сервер коммитит с author-идентичностью из env; committer identity —
    # из конфига репозитория (иначе git commit молча падает).
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
    curl -sS -m 3 "$HEALTH_URL" || echo "(health недоступен)"
    echo
    curl -sS -m 3 "http://127.0.0.1:3000/api/health" || echo "(webui health недоступен)"
}

cmd_logs() {
    "${COMPOSE[@]}" logs -f --tail=50
}

cmd_migrate() {
    ensure_env
    local net
    net="$(docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' slc-mongodb 2>/dev/null | awk '{print $1}')"
    if [ -z "$net" ]; then
        echo "ERROR: контейнер slc-mongodb не найден — легаси-Mongo не запущена" >&2
        exit 1
    fi
    echo "legacy mongo network: $net"
    echo "target vault:         $VAULT_PATH"
    echo "rename ids with LLM:  --rename-with-ai (модель из .env: LMSTUDIO_MODEL)"
    docker run --rm \
        --network "$net" \
        --add-host host.docker.internal:host-gateway \
        --env-file "$ENV_FILE" \
        --entrypoint slc-mcp \
        -v "$VAULT_PATH:/data/vault" \
        slc-mcp:local \
        migrate \
            --from-mongo "mongodb://slc-mongodb:27017" \
            --db slc_mcp \
            --to-vault /data/vault \
            --rename-with-ai
    echo
    echo "Импорт завершён. Перезапустите сервер, чтобы он пересобрал индекс:"
    echo "  ./deploy.sh down && ./deploy.sh up"
    echo "затем пересоберите эмбеддинги: ./deploy.sh reindex"
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
