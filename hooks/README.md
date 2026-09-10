# SLC hooks — auto-save context at the end of an agent loop

`slc-hook.py` is a ready-made hook for a **coding-agent wrapper**: the
harness calls it at the end of every loop iteration (the agent finished a
step/session), and it automatically saves the seat's context to SLC through
the MCP `update_context` tool:

- `save <summary>` — snapshot the context into history (like `/save_context`);
- `update` — just rebuild the current slice (like `/update_context`);
- the summary defaults to the latest git commit (`git log -1` + `diff --stat`);
  you can pass your own or `-` to read it from stdin.

Requirements: `python3` (standard library only) and a running `slc-mcp
serve`. The script does not depend on an LLM server — it works with the hash
mode too (`SLC_LLM=hash`).

## Quick check

```bash
# server: slc-mcp serve --port 3000
export SLC_SEAT_ID=dev
python3 hooks/slc-hook.py save "hook check"
```

The response contains `limit_chars/used_chars/compressed` — if the context
was compressed, a warning appears.

## Wiring into a harness

### Claude Code

`~/.claude/settings.json` (or the project's `.claude/settings.json`):

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "SLC_SEAT_ID=dev python3 /absolute/path/to/slc-hook.py save"
          }
        ]
      }
    ]
  }
}
```

- `Stop` — the end of each agent response in a session; a snapshot goes into
  history.
- To save less often (only at the end of larger stages) use `SubagentStop`
  or call `/save_context` manually.

### ZCode (the client this project is developed in)

ZCode hooks are configured in the client (see the client's hooks help); the
command for the end-of-session/loop event is the same:

```
SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
```

### Any agent / wrapper

A zsh/bash wrapper around your agent works everywhere there is no hook
system:

```bash
agent() {
  "$@"                                   # run the agent
  local rc=$?
  SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
  return $rc
}
# alias claude="agent claude"  etc.
```

### Minimal option — git pre-commit

```bash
# .git/hooks/pre-commit
SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
```

## Environment

| Variable         | Default                       | Description                          |
|------------------|-------------------------------|--------------------------------------|
| `SLC_MCP_URL`    | `http://127.0.0.1:3000/mcp`   | MCP server address                   |
| `SLC_SEAT_ID`    | — (required)                  | seat; can also be passed with `--seat` |
| `SLC_MCP_AUTH`   | `legacy_seat_id`              | `bearer_plus_seat` — if enabled      |
| `SLC_MCP_TOKEN`  | —                             | token for `bearer_plus_seat`         |
| `SLC_SKIP`       | empty                         | any non-empty value exits with 0     |

Different projects = different seats: give each project/repository its own
`SLC_SEAT_ID` (for example in the project's `.env` or in the hook command).
