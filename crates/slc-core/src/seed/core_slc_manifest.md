# SLC-MCP — System Manifest

SLC-MCP — MCP server for AI agent memory management.
Three-tier memory: episodic (diary L1→L4), factual (KB), semantic (embeddings).

## Architecture

- **Engine**: Rust (slc-core + slc-mcp crates)
- **Transport**: MCP Streamable HTTP (JSON-RPC POST) + SSE
- **Storage**: Obsidian vault (markdown, default) or embedded SQLite
- **Embeddings**: LM Studio / Ollama / candle (local). CPU-hash as fallback.
- **MCP Sampling**: inference via MCP client (SSE) when no local LLM. Embeddings unavailable, search degrades to BM25 only.

## Context Model

Context is assembled from blocks by priority (highest → lowest):
1. **Active document** — task/document the agent works on. NEVER dropped.
2. **Focus items** — user's current priorities. NEVER dropped.
3. **Profiles** — seat profile + user profile. Dropped when budget exceeded.
4. **Core documents** — system knowledge. Dropped LAST (least important
   first; the manifest survives longest) when the budget still overflows.

Compression on budget overflow:
- Documents are NEVER truncated — only dropped whole or LLM-summarized.
- Overflow = the seat's TOKEN budget is exceeded.
- Profiles drop first, then core docs one by one from the least important.
- If still over — LLM summarization of remaining docs.
- Response includes `compressed: true` + `warning`.

Budget: `/limit N` (TOKENS; ~3 chars per token) or
`SLC_CONTEXT_LIMIT_TOKENS` env var. Default: 100000 tokens.

## Documents

Each document has:
- `document_id` — unique ID (human-readable or hash)
- `category` — core, module, task, project, documentation, skill, custom, system, history
- `content` — markdown text
- `tags` — search tags
- `folder` — vault path (optional, AI auto-determines via `ai_organize`)
- `seat_id` — if set, document is private to that seat

RAG-eligible: all categories EXCEPT history. History is episodic memory, not indexed.

### auto_load vs references

Documents can have `auto_load` — list of document IDs auto-loaded into context on activation. This is a chain of related documents (e.g., task → project → skills). References are regular links without auto-loading.

### Folders

AI auto-determines folder (`SLC_AI_ORGANIZE=true`):
- `docs/projects/{slug}/` — projects and bound documents
- `docs/skills/` — skills
- `docs/core/` — core documents
- `docs/modules/` — modules
- `tasks/` — standalone tasks
- `history/YYYY/MM/` — episodic records

## Search

### Hybrid Pipeline

1. **Semantic search**: cosine similarity over embeddings (weight: 70%)
2. **Text search**: BM25 over tokenized content (weight: 30%)
3. **Merge**: weighted sum of scores
4. **Relevance gate**: absolute floor (default 0.20) + relative gap to top hit (default 0.6)
5. **Rerank** (optional): blend relevance + importance + recency (30-day half-life) + memory type

### Embeddings

- **Query embedding** — for search queries (model may use different prefix)
- **Passage embedding** — for indexed documents
- Fact deduplication: cosine > 0.92

### Seats and Isolation

- Each seat has a unique ID (caller-supplied or auto-generated)
- Documents with `seat_id` are private to that seat
- Embeddings: `Public` (all seats) vs `Private` (seat-specific)
- SSE events: without `seat_id` → nobody, with `seat_id` → that seat only
- TTL: default 24 hours (`SLC_SEAT_TTL_SECONDS`)

## Tools (MCP Tools)

### Memory
- `remember(event_id, content)` — record episodic event (L1 diary). NOT indexed in RAG.
- `recall(limit?)` — recent episodic history for the seat
- `compress_now` — progressive summarization L1→L4
- `consolidate_now` — extract facts from episodic memory to KB
- `search(query, limit?)` — hybrid search (semantic 70% + BM25 30%)

### Documents
- `add_document(document_id, category, content, folder?)` — add to KB. With `ai_organize` — LLM determines project/folder.
- `get_document(document_id)` — load by ID
- `activate_document(document_id)` — activate as context anchor (included in update_context). Works for ANY category.
- `deactivate_document` — clear active document
- `get_active_document` — current active document
- `update_context(summary?, changes?, decisions?, next_steps?)` — assemble context. With `summary` — saves snapshot to history.

### Tasks
- `create_task(name, description?, project_id?, auto_load?)` — create (private to seat)
- `update_task(task_id, ...)` — update. Statuses: pending/active/completed/cancelled
- `delete_task(task_id)` — delete
- `activate_task(task_id)` — activate (same as activate_document)
- `deactivate_task` — deactivate
- `get_active_task` — current active task
- `list_tasks(project_id?, status?, limit?)` — list

### Projects
- `create_project(name, description?, auto_load?)` — create
- `update_project(project_id, ...)` — update. Statuses: active/archived
- `delete_project(project_id)` — delete
- `get_project(project_id)` — details
- `list_projects(status?, limit?)` — list

### Focus items
- `focus_add(title, description?, priority?, depends_on?, mind_type?)` — add
- `focus_list(mind_type?)` — list. mind_type: front/planner/executor/critic/shared
- `focus_update(focus_id, ...)` — update
- `focus_remove(focus_id)` — remove

### Profiles
- `update_seat_profile(content, timezone?)` — workspace profile (seat)
- `update_user_profile(content)` — user behavioral profile

### Reminders
- `reminder_create(content, remind_at, mind_type?)` — create (ISO-8601 time)
- `reminder_list` — list
- `reminder_cancel(reminder_id)` — cancel
- `pop_notifications(limit?)` — pop pending notifications

### Pagination
- `get_page(response_id, page)` — response page
- `delete_response(response_id)` — delete cached response
- `set_page_limit(page_token_limit)` — page size
- `get_page_settings` — current settings

### Other
- `seat_info` — seat info + usage stats
- `info` — current session: seat, active task
- `command(input)` — slash commands
- `load_module(module_name)` — load public module (stub)

## Slash Commands

| Command | Description |
|---------|-------------|
| `/limit N` | Context limit in TOKENS (~3 chars per token) |
| `/ctx` | Show current context |
| `/search <query>` | Search KB |
| `/update_context [summary]` | Assemble context |
| `/save_context <summary>` | Save context snapshot |
| `/help` | List commands |

## Configuration (env vars)

| Variable | Default | Description |
|----------|---------|-------------|
| `SLC_VAULT_PATH` | `~/.slc/vault` | Vault path |
| `SLC_CONTEXT_LIMIT_TOKENS` | `100000` | Context limit (tokens; ~3 chars each) |
| `SLC_LLM` | auto | Provider: hash/ollama/lmstudio/candle |
| `LMSTUDIO_URL` | — | LM Studio URL |
| `LMSTUDIO_MODEL` | `google/gemma-4-e4b` | Reasoning model |
| `LMSTUDIO_EMBED_MODEL` | `text-embedding-nomic-embed-text-v1.5` | Embedding model |
| `SLC_MCP_SAMPLING` | `false` | Fallback to MCP client inference |
| `SLC_AI_ORGANIZE` | `true` | AI folder determination on add_document |
| `SLC_SEAT_TTL_SECONDS` | `86400` | Seat TTL (0 = never expire) |
| `SLC_SEARCH_MIN_SCORE` | `0.20` | Minimum search score |
| `SLC_SEARCH_MIN_GAP` | `0.6` | Relative gap to top hit |
| `SLC_MCP_AUTH` | `legacy_seat_id` | Auth: legacy_seat_id / bearer_plus_seat / embedded |
| `SLC_MCP_TOKEN` | — | Bearer token for auth |
| `OBSIDIAN_AUTO_GIT_COMMIT` | `false` | Auto git commit on vault writes |

## SSE Events

Server broadcasts via SSE:
- `context_updated` — after update_context (save or compression)
- `document_activated` — after activate_document
- `rpc_response` — JSON-RPC replies (legacy SSE)
- `sampling_request` — MCP sampling requests to client
- `notification` — other notifications

## Background Timers

For each active seat:
- `HISTORY_COMPRESSION` — L1→L4 compression
- `CONSOLIDATION` — fact extraction
- `REMINDER` — reminder firing
- `FOCUS_REMINDER` — focus nudge

## Language Rule

**ALWAYS respond in the same language the user writes in.** If the user writes in Russian — respond in Russian. If in English — in English. If in German — in German. Mirror the user's language. Never switch languages unless the user explicitly asks.

## Key Principles

1. Core documents are SYSTEM knowledge. Never delete without explicit user instruction.
2. Context ≠ entire codebase. It's relevant documents + active task.
3. User edits to core documents are NEVER overwritten on restart.
4. On `compressed: true` — shorten responses, save findings, suggest `/save_context`.
5. Always call `pop_notifications` at the start of every turn.
