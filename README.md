# SLC — Smart Layered Context memory server

SLC is a memory server for AI agents. It gives every agent (or every
human-operated client) a **persistent, shared working memory**: knowledge
documents, projects and tasks, an episodic diary with progressive
summarization, focuses, reminders, and hybrid semantic + BM25 search over all
of it.

The core idea is a **seat** — a stable identity that owns its own documents,
tasks, and context. A coding agent keeps one seat across sessions; its
working context (active document, base knowledge, profiles, focuses) is
assembled on demand, compressed when it does not fit the model window, and
snapshotted back into episodic history when the work is done.

SLC ships as a single self-contained server that speaks three protocols at
once:

- **MCP** (Model Context Protocol, Streamable HTTP) — for agent clients;
- **REST API** — for web UIs, scripts, and integrations;
- **Web UI** — a bundled Svelte SPA served by the same process.

The default storage backend is an **Obsidian vault** (plain markdown files
plus sidecars), so the knowledge base remains human-readable and
git-versioned. SQLite and MongoDB backends are available as alternatives.

---

## How it works

```
┌─────────────────────────────── slc-mcp (one process) ───────────────────────┐
│  MCP  /mcp /sse /messages          REST /api/*          Web UI (SPA) /      │
│        └──────────────┬────────────────┘───────────────────────┘            │
│                 slc-core engine (single owner of the vault)                 │
│                                                                             │
│  Documents · Projects · Tasks │ Episodic history L1→L4 │ Focuses/reminders  │
│  Hybrid search (BM25+vectors) │ Seats & context        │ Auth (JWT/OAuth)   │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                ▼
              Obsidian vault (default) · SQLite · MongoDB
```

Because one process owns the storage, there are no concurrent-access
conflicts: the vault is the single source of truth, and all writes are
serialized through the engine.

### Memory model

- **Everything is a document.** Projects, tasks, skills, and knowledge notes
  share one unified `Document` model with a unique, human-readable id.
  Documents carry `auto_load` (working links pulled into context) and
  `references` (passive mentions).
- **Episodic history** is separate: agents write diary entries
  (`remember`), which are progressively summarized L1 → L2 → L3 → L4 and
  consolidated into permanent facts. History is never embedded or searched —
  it is raw material for reflection.
- **Focuses** are what the agent is concentrating on (with priorities and
  dependencies); **reminders** schedule one-shot or periodic callbacks.
- **Seat context** is assembled by `update_context`: the active document,
  base knowledge, profiles, and focuses, trimmed to the token budget by
  dropping low-priority blocks first and, when needed, LLM-compressing the
  rest — never by truncating documents by hand.

---

## Quick start

### Docker (recommended)

```bash
cp .env.example .env          # set SLC_VAULT_PATH, LLM provider, etc.
./deploy.sh up                # builds the image, inits the vault, starts
```

The server listens on `http://127.0.0.1:3000`.

### From source

```bash
cargo run -p slc-mcp -- serve --port 3000
```

### First MCP call

```bash
curl -X POST http://127.0.0.1:3000/mcp \
  -H 'Content-Type: application/json' \
  -H 'X-Seat-ID: my-agent' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"add_document",
                 "arguments":{"document_id":"hello_world","category":"custom",
                              "content":"Hello from SLC"}}}'
```

The seat is created on first use and persists. Point an MCP client at
`http://host:3000/mcp` (or the `/sse` stream) and the full tool catalog
appears automatically.

---

## Configuration

Copy `.env.example` to `.env`. Key variables:

| Variable | Default | Purpose |
|---|---|---|
| `SLC_VAULT_PATH` | `~/.slc/vault` | Obsidian vault (or SQLite file) location |
| `SLC_LLM` | — | Provider: `lmstudio`, `ollama`, `candle`, `hash` |
| `SLC_MCP_SAMPLING` | `false` | Delegate reasoning to the connected MCP client (`sampling/createMessage`); no generative endpoint needed. `SLC_LLM=hash` is then the fail-closed fallback for CLI paths |
| `LMSTUDIO_URL` / `LMSTUDIO_MODEL` / `LMSTUDIO_EMBED_MODEL` | — | OpenAI-compatible LM Studio endpoint and models |
| `OLLAMA_ENDPOINT` / `OLLAMA_REASONING_MODEL` / `OLLAMA_EMBEDDING_MODEL` | — | Ollama fallback |
| `SLC_AI_ORGANIZE` | `true` | Ask the reasoning LLM where a new document belongs (project folder) |
| `OBSIDIAN_AUTO_GIT_COMMIT` | `false` | Commit vault changes to git after writes |
| `SLC_MCP_AUTH` | `legacy_seat_id` | MCP auth: `legacy_seat_id`, `bearer_plus_seat`, `embedded` |
| `SLC_MCP_TOKEN` | — | Bearer token when `SLC_MCP_AUTH=bearer_plus_seat` |
| `SLC_AUTH` | `seat` | Web-UI/REST auth: `seat` or `full` (users + JWT + Yandex OAuth) |
| `SLC_SEAT_TTL_SECONDS` | `86400` | Seat expiry; `0` = never |
| `SLC_SEAT_ROLES` | — | Seat roles, e.g. `boss=operator` |
| `SLC_SEAT_MANAGE_ACL` | — | Explicit cross-seat targets, e.g. `boss=worker|tester` |
| `SLC_CONTEXT_LIMIT_TOKENS` | `100000` | Fallback token budget for `update_context` |
| `SLC_PAGINATION_ENABLED` | `true` | Paginate oversized tool responses |
| `SLC_PAGE_TOKEN_LIMIT` | `50000` | Approximate page size in tokens |
| `JWT_SECRET_KEY` | — | JWT signing key (required for `SLC_AUTH=full`) |
| `YANDEX_CLIENT_ID` / `YANDEX_CLIENT_SECRET` | — | Yandex OAuth credentials for the web UI |

---

## MCP server

Endpoints (Streamable HTTP, MCP `2025-03-26`):

| Endpoint | Purpose |
|---|---|
| `POST /mcp` | JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`, …) |
| `GET /sse`, `POST /messages` | Streamable HTTP SSE transport |
| `GET /health` | Health check |

Auth is per-request: the client sends its seat in the `X-Seat-ID` header
(`SLC_MCP_AUTH=legacy_seat_id`, the default). In `bearer_plus_seat` mode a
bearer token is required as well.

With `SLC_MCP_SAMPLING=true`, reasoning requests (compression,
consolidation, AI id naming) are returned to the authenticated seat's
connected MCP client as `sampling/createMessage` — SLC then needs no
dedicated generative endpoint; embeddings degrade to BM25 text search.

### Tool groups

- **Knowledge**: `add_document`, `get_document`, `update_document`,
  `delete_document`, `list_documents`, `rename_document`, `search`
- **Tasks & projects**: `create_task`, `update_task`, `delete_task`,
  `activate_task`, `rename_task`, and the matching `*_project` tools
- **Memory**: `remember`, `recall`, `compress`, `consolidate`
- **Context**: `update_context`, `save_context`, slash commands (`/ctx`,
  `/limit N`, `/search …`)
- **Focuses & reminders**: `focus_add` / `focus_update` / `focus_remove`,
  `reminder_create` / `reminder_cancel`, notifications
- **Seats**: `seat_roles`, cross-seat activation via `target_seat`
- **External state**: `state_get` / `state_put` / `state_list` /
  `state_delete` — seat-scoped, optimistic-concurrency text objects
- **Pagination**: `get_page`, `delete_response`, `set_page_limit`,
  `get_page_settings`

### Editing documents

Bodies are edited with **diff operations only** (`diff`): `append`,
`prepend`, `replace_section`, `remove_section`, addressed by markdown
headings. Full-body replacement is not exposed for updates; the legacy
`description` / `description_patch` parameters are still accepted for
compatibility with older agent instructions, but new clients should use
`diff`.

Renames (`rename_document`, `rename_task`, `rename_project`) cascade: they
rewrite `auto_load`/`references`, project bindings, `[[wiki-links]]` in body
text, active seat pointers, and embeddings.

### Pagination

Large tool responses are paginated instead of truncated. The first page
carries `_pagination` (`response_id`, `page`, `total_pages`) and an explicit
instruction to fetch the rest with `get_page(response_id=…, page=2..N)`.
Documents larger than a page are split by content into parts (`part: "k/n"`)
that reassemble losslessly.

If your client's harness truncates tool output, lower the page size:

- per connection: header `X-SLC-Page-Token-Limit: <tokens>`;
- persisted: `set_page_limit(<tokens>)` (or env `SLC_PAGE_TOKEN_LIMIT`).

A page of `N` tokens ≈ `N×3` characters ≈ `N×9` bytes of Cyrillic text.
The default page size is `50000` tokens (large responses stay single-page
unless they are truly huge); if your client harness truncates output at a
byte budget, lower the limit to roughly `budget_bytes / 9` (e.g. `5000` for
a 50 KB budget).

---

## Seats, roles and cross-seat management

A **seat** is the unit of ownership and context. Clients authenticate as a
seat, and all their documents, tasks, focuses, and context belong to it.

Seats can be granted roles:

- `operator` — may manage the *context* of other seats: activate/deactivate
  their documents, tasks, projects, and focuses (tools accept a
  `target_seat` argument).

A role alone grants nothing across seats: an operator must also have an
explicit target in `SLC_SEAT_MANAGE_ACL`, e.g.
`SLC_SEAT_MANAGE_ACL=planner=worker_a|worker_b` or `root=*`. This keeps an
accidentally configured `operator` from becoming a global administrator.

---

## Web UI and REST API

The same process serves a Svelte 5 web UI and a REST API (`/api/*`):

- documents, tasks, projects, seats, search, stats, context, notifications,
  reminders, focuses — with CRUD where applicable;
- `GET /api/events` — SSE notification stream;
- `GET /api/auth/…` — user authentication endpoints.

Two auth modes for the web layer (`SLC_AUTH`):

- `seat` (default): the browser keeps a seat id (header or cookie); anyone
  with network access can use the service — suitable for trusted networks.
- `full`: real user accounts. Users register or log in (password, bcrypt),
  receive JWT access/refresh tokens with rotation, and may sign in via
  **Yandex OAuth** with an allowlist (explicit rules or organization
  membership via Yandex 360 Directory). Admin endpoints manage users,
  groups, and OAuth rules. In this mode every API request is tied to the
  user's own seat.

---

## Storage backends

| Backend | When to use |
|---|---|
| **Obsidian vault** (default) | Human-readable markdown + frontmatter, git-versioned, editable in Obsidian |
| SQLite | Embedded single-file storage |
| MongoDB | Legacy deployments; migration source |

The vault layout is hierarchical: `docs/<category>/…` for knowledge,
`docs/projects/<project>/…` for project-scoped documents, `seats/` for seat
records, and `.slc/` for indexes, embeddings, and auth state.

---

## CLI

The same binary doubles as a CLI for maintenance and scripting:

```bash
slc-mcp serve [--port 3000] [--auto-commit]   # run the server
slc-mcp status                                 # backend + health
slc-mcp search "query" --seat SEAT_ID          # hybrid search
slc-mcp remember SEAT_ID EVENT_ID "text"       # write a diary entry
slc-mcp compress SEAT_ID                       # run L1→L4 summarization
slc-mcp consolidate SEAT_ID                    # extract permanent facts
slc-mcp migrate --from-mongo URI --db slc_mcp  # import a legacy deployment
slc-mcp reindex-embeddings                     # rebuild vectors after model change
slc-mcp init                                   # provider/model setup wizard
```

### Migrating from the legacy Python SLC

```bash
slc-mcp migrate --from-mongo mongodb://HOST:27017 --db slc_mcp \
  --to-vault /path/to/slc-vault [--rename-with-ai]
```

Legacy ids are preserved by default so existing seat pointers and references
stay valid; `--rename-with-ai` renames ids to human-readable slugs with the
reasoning LLM and rewrites all links. After a bulk import, reindex
embeddings and review the migration report before the first vault commit.

---

## Development

```text
crates/slc-core   engine library (also built as a staticlib for embedding)
crates/slc-mcp    server binary: MCP + REST + web UI + CLI
web-ui/           Svelte 5 SPA
```

```bash
cargo build -p slc-mcp
cargo test -p slc-core -p slc-mcp
```

Packaging: the Dockerfile consumes a `.deb` produced locally
(`cargo deb -p slc-mcp -o dist`, plus the SPA built into `web-ui/dist`), so
the image itself contains no toolchain. `docker-compose.yml` builds from
source for local development.

## License

MIT
