# SLC MCP

Rust MCP server for shared agent context: knowledge documents, projects,
tasks, episodic history, focuses, reminders, hybrid semantic/BM25 search, and
per-seat working context. Obsidian is the default storage backend.

## Local run

```bash
cp .env.example .env
cargo run -p slc-mcp -- serve --port 3000
```

The service exposes:

- MCP: `POST /mcp`
- health: `GET /health`
- REST/UI: `GET /api/*` and `/`

Set `SLC_VAULT_PATH` to the Obsidian vault. Inference is configured with
`SLC_LLM` and the matching provider variables; the development swarm uses the
OpenAI-compatible LM Studio endpoint and models from `.env`.

The MCP endpoint is standard Streamable HTTP and currently negotiates MCP
`2025-03-26`. It returns an object result for legacy `ping` requests and
accepts JSON-RPC notifications with HTTP 202 and an empty body. No Hermes
patch is required: compatibility is verified with the official MCP SDK as an
independent client. Newer protocol revisions that omit `ping` continue to use
the same initialize/tools flow.

The generic seat-scoped `state_get`, `state_put`, `state_list`, and
`state_delete` tools expose optimistic-concurrency text objects for external
memory, skills, or other clients. They are ordinary MCP tools, not a
Hermes-specific transport. `expected_etag` prevents silent concurrent
overwrites; SLC also isolates every object by the authenticated `X-Seat-ID`.

## agent-dev-0 deployment

The development swarm uses:

- source: `/opt/demlabs-dev-swarm/slc-mcp`
- container: `dev-swarm-slc-mcp`
- vault: `/opt/demlabs-dev-swarm/slc-vault`
- MCP from swarm containers: `http://slc-mcp:3000/mcp`
- host MCP: `http://127.0.0.1:3000/mcp`
- web UI: `http://agent-dev-0:2002`
- private Forgejo repository: `devops/slc-vault`

Deploy with:

```bash
docker compose -f docker-compose.agent-dev.yml up -d --build
```

The vault is an independent Git repository. Runtime Git access uses a
write-enabled deploy key from `slc-mcp/secrets/`; secrets and the vault itself
must never be committed to this source repository. With
`OBSIDIAN_AUTO_GIT_COMMIT=true`, document and operational sidecar writes are
committed and pushed automatically. Volatile seat access heartbeats are
coalesced to avoid a push for every read-only MCP request.

## Seats and lifecycle

The swarm authenticates with `SLC_MCP_AUTH=legacy_seat_id`. Every role sends
its unique stable seat in the `X-Seat-ID` header. The manager has the
`operator` role, but cross-seat access remains fail-closed: a target must also
be listed explicitly in `SLC_SEAT_MANAGE_ACL`.

Hermes hooks call `update_context` before every model iteration and
`save_context` afterward. Cron runs and delegated subagents use the same
hooks. Developer and junior DeepSeek Harness runners receive the SLC URL and
seat header from their wrapper, which also records runner-start and runner-end
lifecycle snapshots. Hermes' built-in memory and agent-created skill tree use
the same seat through the generic external-state tools; the container-local
writable tree is only a process cache.

## Mongo migration

Back up MongoDB first, then import into an empty vault:

```bash
slc-mcp migrate \
  --from-mongo mongodb://HOST:27017 \
  --db slc_mcp \
  --vault /path/to/slc-vault
```

Legacy document, task, and project IDs are preserved by default so active
seat pointers and references remain valid. Use `--rename-with-ai` only for an
explicit, reviewed rename migration. Disable automatic Git commits during a
bulk import, reindex embeddings afterward, inspect the migration report, and
create the initial vault commit only after validation.

Useful commands:

```bash
slc-mcp reindex-embeddings
slc-mcp search "query" --seat SEAT_ID
slc-mcp status
```

The migrated legacy Mongo data should be retained separately for rollback;
the production service does not need Mongo after a successful cutover.
