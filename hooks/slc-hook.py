#!/usr/bin/env python3
"""SLC hook — авто-сохранение контекста в конце агент-лупа.

Вызывает MCP-тул `update_context` сервера slc-mcp по JSON-RPC (POST /mcp).
Предназначен для обвязки кодинг-агента: харнес дёргает его в конце каждого
цикла (Stop/PostToolUse, конец сессии, обёртка вокруг агента) — контекст
сита сохраняется в SLC автоматически.

Примеры:
  slc-hook.py save "добавил fastembed-фоллбэк в slc-core"   # снимок с саммари
  slc-hook.py save                                           # саммари из git
  slc-hook.py update                                         # просто собрать контекст
  echo "сделал X" | slc-hook.py save -                       # саммари из stdin

Окружение:
  SLC_MCP_URL     адрес MCP-сервера (default http://127.0.0.1:3000/mcp)
  SLC_SEAT_ID     сид (обязателен, если не задан --seat)
  SLC_MCP_AUTH    legacy_seat_id (default) | bearer_plus_seat
  SLC_MCP_TOKEN   bearer-токен, только для bearer_plus_seat
  SLC_SKIP        непустое значение — выйти 0 без вызова (лёгкий on/off)

Использует только стандартную библиотеку Python 3.
"""

import argparse
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

DEFAULT_URL = "http://127.0.0.1:3000/mcp"


def env_bool(name: str) -> bool:
    v = os.environ.get(name, "")
    return v.strip().lower() not in ("", "0", "false", "no", "off")


def git_summary() -> str:
    """Собрать саммари из последнего коммита + diff --stat (если это git-репо)."""
    try:
        log = subprocess.run(
            ["git", "log", "-1", "--format=%s%n%b"],
            capture_output=True, text=True, timeout=10,
        ).stdout.strip()
        stat = subprocess.run(
            ["git", "diff", "--stat", "HEAD~1..HEAD"],
            capture_output=True, text=True, timeout=10,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""
    if not log:
        return ""
    parts = [log]
    if stat:
        parts.append(stat)
    return "\n".join(parts)[:2000]


def build_summary(args: argparse.Namespace) -> str:
    summary = args.summary
    if summary == "-":
        return sys.stdin.read().strip()[:2000]
    if summary:
        return summary
    s = git_summary()
    if s:
        return s
    return f"конец агент-лупа ({datetime.now(timezone.utc).isoformat(timespec='seconds')})"


def rpc(url: str, seat: str, token: str | None, method: str, params: dict) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "X-Seat-ID": seat,
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = resp.read().decode()
    except urllib.error.HTTPError as e:
        # JSON-RPC errors over HTTP still carry a body; try to surface it.
        detail = ""
        try:
            detail = e.read().decode()[:500]
        except Exception:
            pass
        raise RuntimeError(f"HTTP {e.code} от {url}: {detail or e.reason}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"не удалось подключиться к {url}: {e.reason}") from e

    # SSE-обёртка (data: {...}) — на случай, если сервер вернёт event-stream.
    if data.lstrip().startswith("data:"):
        data = "\n".join(
            line[5:].strip() for line in data.splitlines() if line.startswith("data:")
        )
    msg = json.loads(data or "{}")
    if "error" in msg and msg["error"]:
        err = msg["error"]
        raise RuntimeError(f"MCP error {err.get('code')}: {err.get('message')}")
    return msg.get("result") or {}


def tool_text(result: dict) -> str:
    content = result.get("content") or []
    text = ""
    for part in content:
        if isinstance(part, dict) and part.get("type") == "text":
            text += part.get("text", "")
    return text.strip()


def main() -> int:
    if env_bool("SLC_SKIP"):
        return 0

    parser = argparse.ArgumentParser(
        prog="slc-hook.py",
        description="SLC hook: сохранить/обновить контекст в конце агент-лупа.",
    )
    parser.add_argument(
        "action", choices=["save", "update", "ctx"],
        help="save — снимок в историю (update_context со summary); update/ctx — собрать срез",
    )
    parser.add_argument("summary", nargs="?", default=None,
                        help="саммари для снимка; '-' — из stdin; пусто — саммари из git")
    parser.add_argument("--seat", default=os.environ.get("SLC_SEAT_ID"),
                        help="сид (перекрывает SLC_SEAT_ID)")
    parser.add_argument("--url", default=os.environ.get("SLC_MCP_URL", DEFAULT_URL),
                        help="адрес MCP-сервера (перекрывает SLC_MCP_URL)")
    args = parser.parse_args()

    if not args.seat:
        print("slc-hook: нет SLC_SEAT_ID (задай env или --seat)", file=sys.stderr)
        return 2

    token = None
    if os.environ.get("SLC_MCP_AUTH") == "bearer_plus_seat":
        token = os.environ.get("SLC_MCP_TOKEN")
        if not token:
            print("slc-hook: SLC_MCP_AUTH=bearer_plus_seat, но нет SLC_MCP_TOKEN", file=sys.stderr)
            return 2

    try:
        if args.action == "save":
            summary = build_summary(args)
            result = rpc(args.url, args.seat, token, "tools/call", {
                "name": "update_context",
                "arguments": {"summary": summary},
            })
            print(f"[slc-hook] снимок сохранён (seat={args.seat})")
        else:
            result = rpc(args.url, args.seat, token, "tools/call", {
                "name": "update_context",
                "arguments": {},
            })
            print(f"[slc-hook] контекст обновлён (seat={args.seat})")
    except RuntimeError as e:
        print(f"slc-hook: {e}", file=sys.stderr)
        return 1

    text = tool_text(result)
    if text:
        # Сводка результата — limit/used/compressed предупреждение не теряем.
        tail = [ln for ln in text.splitlines() if ln.strip()]
        print("\n".join(tail[:12]) if tail else text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
