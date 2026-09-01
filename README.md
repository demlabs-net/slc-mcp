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

Set `SLC_VAULT_PATH` to the Obsidian vault. The development swarm enables
`SLC_MCP_SAMPLING=true`: seat-scoped reasoning is delegated to the model of the
connected MCP client with `sampling/createMessage`, so SLC has no dedicated
generative endpoint. `SLC_LLM=hash` is the fail-closed provider for CLI paths
that run without a connected client. Search itself remains available through
BM25; agents perform multi-hop retrieval by iterating the search and read tools.
Standalone installations may instead configure `SLC_LLM` and the matching
LM Studio, Ollama, or Candle provider variables.

The MCP endpoint is standard Streamable HTTP and currently negotiates MCP
`2025-03-26`. It returns an object result for legacy `ping` requests and
accepts JSON-RPC notifications with HTTP 202 and an empty body. No Hermes
patch is required: compatibility is verified with the official MCP SDK as an
independent client. Newer protocol revisions that omit `ping` continue to use
the same initialize/tools flow.

`update_context` applies only the seat's configured context-token budget. It
does not invoke an LLM merely to fit a Hermes- or vendor-specific output cap.
Large list-shaped tool results can use SLC's advertised `get_page` extension;
the extension is carried in ordinary MCP `TextContent` and needs no transport
patch in the client. When more than one page exists, page one ends with an
explicit instruction to retrieve every remaining page in order before acting
on the result.

Pagination is enabled by default. `SLC_PAGINATION_ENABLED` toggles it for the
server and `SLC_PAGE_TOKEN_LIMIT` sets both the approximate page size and the
threshold at which a list response is paginated (default `50000` tokens).
`set_page_limit` persists the default when no environment override is present.
An MCP client may override response shaping for only its own HTTP connection:

- `X-SLC-Pagination: enabled|disabled`
- `X-SLC-Page-Token-Limit: <tokens>`
- `X-SLC-Context-Token-Limit: <tokens>` sets the `update_context` and
  `save_context` budget for that connection without changing the seat-wide
  value.

`SLC_CONTEXT_LIMIT_TOKENS` is a fallback, not a maximum (default `100000`).
Clients should choose their own budget explicitly through the connection
header or persist a seat-specific value with `command {"input":"/limit N"}`.
For example, deployments may choose 50K, 100K, or 300K for different model
windows; these are configuration examples and are not model tiers built into
SLC.

These are optional HTTP transport headers; the JSON-RPC/MCP message format is
unchanged, so clients using the official SDK remain fully compatible.

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
- inference: seat-scoped MCP sampling; no dedicated LM Studio/Ollama model

Deploy with:

```bash
./scripts/build-deb.sh
docker compose -f docker-compose.agent-dev.yml up -d --build
```

The build script runs the Rust workspace tests and packages the exact checkout
into the ignored `dist/` directory consumed by the runtime image.

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
hooks. The Developer harness receives the SLC URL and seat header from its
wrapper, which also records runner-start and runner-end lifecycle snapshots.
Junior roles work directly through Hermes and share the same lifecycle hooks.
Hermes' built-in memory and agent-created skill tree use the same seat through
the generic external-state tools; the container-local writable tree is only a
process cache.

## Durable task workflow

SLC is the source of truth for delegated work. `assign_task` creates the task
with stable issuer/assignee principals and parent/root lineage; `start_task`,
`report_task`, `cancel_task`, and `task_message` append immutable events and
update the task projection. `get_task`, workflow-aware `list_tasks`, and
`list_task_events` work without any message bus. Idempotency keys make assignment and event
retries safe. Their durable task/event IDs also repair a crash between the
primary write and its portable-backend idempotency index. Old retries do not
rewind the latest-event cursor, while their monotonic status transition is
still repaired if the crash happened before the projection write. Workflow
mutations are serialized so concurrent retries cannot create duplicate events.
Caller-owned assignment metadata is isolated from SLC projection fields and
returned as `task.metadata`.

Every assignee has a durable FIFO with capacity one. A new assignment is
`ready` only when it owns the lane; later work remains `queued`. `start_task`
rejects queued work, and every progress or terminal report requires the task
to be running, so a queued task cannot close past the FIFO head. A terminal
report releases the lane; `reconcile_task_queue` then promotes the oldest
queued task and repairs interrupted projection writes or duplicate `ready`
reservations. A non-running `ready` reservation that is not the oldest item is
demoted and the true head is restored. Multiple actual `running` writers remain
a hard error because choosing one automatically would be unsafe. Backend filtering happens before
the 5000-item per-assignee safety bound, preventing unrelated or old terminal
tasks from truncating a role queue silently. The queue is per agent profile,
independently of how many global parallel slots its inference backend exposes.
Assignment idempotency remains replayable after terminal tasks leave the
runnable FIFO. `cancel_task` lets the issuer, assignee, or global coordinator
remove queued/ready work without starting it; cancelling a reserved head also
promotes the next FIFO item.

Configure `SLC_PRINCIPAL_SEATS` to map transport-neutral participant names to
the existing SLC seats, and configure delegation independently with
`SLC_TASK_ASSIGN_ACL`. A global coordinator requires an explicit `"*"` grant.
`SLC_TEXT_ONLY_PRINCIPALS` prevents non-vision models from claiming visual
acceptance while still allowing them to report hashes, dimensions, and other
machine evidence.

A delivery adapter is optional. Assignment, reconciliation, message, and
report results include a sibling `wake_recommended` flag and, when applicable,
a four-field
`delivery` envelope: recipient, opaque task correlation ID, stable
event-derived idempotency key, and a content-free instruction to read SLC.
When a wake is recommended, a caller may pass `delivery` unchanged through
Swarm MCP, Matrix, or another adapter. A queued assignment returns
`wake_recommended=false` and must not be delivered until reconciliation exposes
it as the single ready head. The task description, message, and
report body remain only in SLC. Replacing the adapter therefore does not
migrate task state or conversation history.

Cancellation never recommends waking the cancelled task itself. If cancelling
the reserved head exposes another task, `next_wake_recommended=true` and the
separate `next_delivery` envelope identify the only run that should be started.

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
