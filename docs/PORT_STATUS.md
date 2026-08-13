# SLC Rust — порт-статус: сверка с легаси (Python)

Сверка фич `slc-mcp` (Rust) против легаси-движка (`slc-mcp.legacy` →
`/Users/dmitriygerasimov/work/ai/slc`), по состоянию на 2026-08-13.

Легенда: ✅ сделано · 🟡 частично · ⛔ не портировано.

## Хранилище

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| StorageBackend абстракция | Mongo + Obsidian | ✅ trait | kb/episodic РАЗДЕЛЕНЫ (лучше легаси — инвариант типов) |
| Obsidian vault (markdown + frontmatter) | ✅ | ✅ | + папки, человекочитаемые имена, history/YYYY/MM, git auto-commit |
| SQLite (встроенный) | нет | ✅ | Rust-only бонус (эмбеддинг в приложение) |
| External sync (git-remote KB) | ✅ | ⛔ | `storage/obsidian/sync.py` + 4 MCP-тула — не портирован |
| Backup/restore (JSON, admin) | ✅ | ⛔ | экспорт/импорт 5 коллекций, whitelist, upsert-merge |

## Документы (единая модель Document)

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| Единая сущность (проекты/задачи/доки) | 🟡 отдельные коллекции | ✅ | Rust строго лучше: одно `Document` |
| id = уникальное ИМЯ (не хэш) | 🟡 hash-иды | ✅ | content_hash только для dedup |
| CRUD (insert/get/update/upsert/find/count) | ✅ | ✅ | + version-счётчик |
| Soft delete / graveyard / restore / purge | ✅ | ✅ | 30-дн. cleanup |
| auto_load / references (поля) | ✅ | ✅ | хранятся |
| link/unlink (auto_load|references) | ✅ | ⛔ | API отсутствует |
| get_document_with_references (traversal) | ✅ | ⛔ | BFS по auto_load, max_depth — нет |
| search_and_replace (regex) | ✅ | ⛔ | |
| list (tags_match/skip/pagination/содержимое) | ✅ | 🟡 | kb_find есть, но без пагинации/превью |
| document history/rollback/diff | ✅ | ⛔ | только version-счётчик |
| импорт batch/json/markdown | ✅ | 🟡 | import одного файла в CLI; batch нет |

## Поиск и эмбеддинги

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| Гибридный поиск (семантика + текст) | ✅ | ✅ | cosine + BM25 |
| Inverted reranker (importance/recency/mem_type) | ✅ | ✅ | env-веса |
| Semantic fallback → text-only | ✅ | ✅ | |
| Чанкинг эмбеддингов 1800/200 | ✅ | ⛔ | Rust эмбеддит целиком (1 чанк) |
| scopes public/private | ✅ | ✅ | |
| generate_for_all_missing / rebuild | ✅ | ⛔ | |
| Субагентный (agentic) поиск | ✅ | ⛔ | SearchAgent + run-поток — нет (модель уже выбрана: gemma-4-e4b) |

## Память (движок)

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| 4 уровня: Working/Episodic/Semantic/Procedural | ✅ | ✅ | Procedural = profiles/instructions — 🟡 |
| remember (L1) / recall | ✅ | ✅ | |
| Progressive summarization L1→L2→L3→L4 | ✅ | ✅ | порог 7/4, merge-upsert L4 |
| Консолидация фактов (+dedup 0.92, cap 200) | ✅ | ✅ | **фикс**: `consolidated=true` теперь пишется (в легаси — баг, источники пере-обрабатывались) |
| Reflection (идеи из истории+фокусов) | ✅ | ⛔ | |
| Focus-система (decay, depends_on, max 7) | ✅ | ⛔ | |
| Idea pool (weighted random, activation 0.75) | ✅ | ⛔ | |
| Reminders (NL-парсер, recurrence) | ✅ | ⛔ | |
| Notifications (queue, TTL, push) | ✅ | ⛔ | |
| Personalization (User/Seat profiles) | ✅ | ⛔ | Procedural-слой памяти |

## Таймеры

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| PersistedTimer (модель + storage) | ✅ | ✅ | |
| TimerRegistry (планировщик, tokio) | ✅ | ⛔ | есть storage-методы, нет фонового цикла |
| Дефолтные таймеры на seat (900/2700/7200/86400/86400) | ✅ | ⛔ | |
| cancel/pause/resume/restart | ✅ | ⛔ | |
| Хендлеры: compression/consolidation/reflection/reminder | ✅ | 🟡 | compression+consolidation — run-now; авто-расписание — нет |

## Seat'ы

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| create/get/list/close/cleanup_expired | ✅ | ✅ | |
| usage_stats (requests/tokens/tools) | ✅ | ✅ | |
| active_task_id + контекст | ✅ | 🟡 | set_active_task есть, в MCP/CLI не выведен |
| limits/metrics overview | ✅ | ⛔ | |

## MCP-сервер

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| Транспорт streamable-HTTP | ✅ | 🟡 | JSON-RPC over HTTP; SSE нет |
| Auth: legacy_seat_id / bearer / embedded | ✅ | 🟡 | только X-Seat-ID (+401 без него); пермишены `*:*`/`knowledge:public:write`/`settings:write` — нет |
| Инструменты | ~40 | 🟡 | 8: search/get_document/add_document/remember/recall/seat_info/compress_now/consolidate_now |
| Пагинация ответов (_pagination) | ✅ | ⛔ | |
| Инъекция pending-уведомлений | ✅ | ⛔ | |
| Prompts (check_notifications/session_briefing/save_session) | ✅ | ⛔ | |
| Связывание сессии MCP ↔ seat | ✅ | ⛔ | |
| Запись tool-call в stats/activity | ✅ | 🟡 | usage_stats пишется; activity timeline — нет |

## Агенты, REST, UI

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| AgentRegistry + runs (LangGraph) | ✅ | ⛔ | Phase 7 «агент» — большой блок, отдельно |
| REST API (/api/*) | ✅ | ⛔ | admin/auth/kb/seats/tasks/agents/activity… |
| Web UI (Svelte 5) | ✅ | ⛔ | в плане Vassista — clients/ позже |
| Observability (ActivityRecorder, MemoryMetrics) | ✅ | ⛔ | |

## LLM

| Фича | Легаси | Rust | Примечание |
|---|---|---|---|
| LM Studio (OpenAI-compat) | ✅ | ✅ | gemma-4-e4b / nomic-embed-v1.5, .env |
| Ollama | ✅ | ✅ | |
| MockLlm (тесты) | — | ✅ | Rust-only |

## Выводы

Скопирована **сердцевина**: хранилище (Obsidian vault как дефолт), единая
модель Document, гибридный поиск с реранкером, пайплайн памяти L1→L4 +
консолидация (с фиксом), seat'ы, MCP-скелет, оба LLM-провайдера.

**Не портировано (по приоритету для Phase 7):**
1. TimerRegistry (авто-расписание compression/consolidation) — движок памяти
   без него не «живёт» сам.
2. Reflection + Focus + Idea pool — второй контур памяти (идеи/фокусы).
3. Reminders + Notifications — UX-канал памяти.
4. MCP: остальные ~32 тула, пагинация, auth-режимы/пермишены, prompts, SSE.
5. Backup/restore, link/unlink + auto_load traversal, search_and_replace,
   чанкинг эмбеддингов 1800/200.
6. Personalization (профили), observability, External sync.
7. Агенты (registry/runs), REST API, Web UI — отдельные крупные блоки.
