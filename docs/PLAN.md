# SLC MCP — план работ (Rust-порт движка памяти)

**Владелец:** этот агент. **Скоуп:** только `slc-mcp` (+ `vs-memory` как
точка встраивания). Всё остальное (Phase 1 audio, LLM-плагины, s2s и т.д.) —
другие агенты, сюда не лезем.

База сверки: `docs/PORT_STATUS.md` (что уже перенесено из Python-легаси).
Правила дизайна (не нарушать): единый `Document`; id = уникальные имена;
история отдельно от KB (не RAG); Obsidian vault — дефолтный бэкенд; двойная
упаковка (бинарь + static lib через vs-memory).

## P0 — TimerRegistry (авто-жизнь движка памяти) ✅
- [x] `timer.rs`: TimerRegistry — tokio-задачи по активным таймерам
      (sleep_until → set_timer_fired → handler → перепланировка periodic)
- [x] дефолтные таймеры на seat (env-интервалы: FOCUS 900 / IDEA 2700 /
      REFLECTION 7200 / COMPRESSION 86400 / CONSOLIDATION 86400)
- [x] handlers: HISTORY_COMPRESSION → HistoryCompressor,
      CONSOLIDATION → MemoryConsolidator (остальные — по мере P1/P2)
- [x] `SlcEngine::start_background()` + вызов в `serve`; тест с
      `tokio::time::pause/advance` (start_paused + advance_in_steps)

## P1 — Второй контур памяти: Reflection + Focus + Ideas ✅
- [x] `focus.rs`: FocusManager (MAX 7, decay λ=0.02, depends_on + cycle-check,
      auto_archive, mind_type из proactivity_scope)
- [x] `ideas.rs`: IdeaPool (MAX 50, weighted random, activation cosine ≥0.75,
      reminded_count, auto_archive)
- [x] `reflection.rs`: ReflectionEngine (последние 10 L1/L2 + активные фокусы →
      LLM JSON [идеи ≤5] → IdeaPool.add(source=reflection))
- [x] `proactivity.rs`: MindType + normalize_write_mind_type / mind_matches
      (per-mind scoping: front/planner/executor/critic/shared)
- [x] storage: `list_records` / `delete_record` в StorageBackend + obsidian/sqlite
- [x] MCP-тулы: focus add/list/remove/update, idea add/list/remove/get_random,
      reflect_now; `SlcEngine` facade + REFLECTION handler в start_background

## P2 — Reminders + Notifications (UX-канал) ✅
- [x] `reminders.rs`: ReminderManager (+ ISO/RFC3339-парсер remind_at; NL `dateparser` не портирован — возвращает ошибку)
- [x] `notifications.rs`: Notification + очередь (pending→delivered), TTL 24h
- [x] handler'ы REMINDER / FOCUS_REMINDER / IDEA_REMINDER → push Notification
      (в `start_background`); `TimerRegistry::register` теперь спавнит таск
- [x] MCP-тулы reminders (create/list/cancel) + pop_notifications +
      prompts `check_notifications` + инъекция pending-уведомлений в ответы тулов

## P3 — MCP-сервер: полнота + auth + пагинация
- [ ] тулы: tasks (create/update/delete/activate/deactivate/get_active/list),
      projects (5), profiles (user/seat), info, context (update_context/load_module),
      chunks/pagination (get_page/delete_response/set_page_limit)
- [ ] пагинация ответов (`_pagination` envelope, list-ключи)
- [ ] auth: bearer_plus_seat + embedded режимы, пермишены
      (`knowledge:public:write`, `settings:write`, `*:*`), связывание сессии
- [ ] инъекция pending-уведомлений в ответы тулов
- [ ] SSE-транспорт (streamable HTTP) — отдельно, P3.5

## P4 — KB-доводка
- [ ] **diff-обновления документов**: `kb_patch` принимает **diff** (отдельные
      поля/фрагменты: append/insert/replace по ключам или линейным меткам),
      а НЕ всё тело заново. Целостность версии/updated_at; оптимистичный
      конфликт-контроль (version-match). Применяется и к task/project update.
- [ ] link/unlink (auto_load|references) + get_document_with_references (BFS)
- [ ] search_and_replace (regex, dry-run)
- [ ] чанкинг эмбеддингов 1800/200 + generate_for_all_missing
- [ ] backup/restore (JSON-экспорт/импорт KB+embeddings+seats+tasks)
- [ ] list с пагинацией/превью/tags_match

## P4.1 — Истории (эпизодическая память) — неизменяемые
- [ ] **read-only эпизодический API**: отдельные тулы/ф-ии поиска и чтения
      историй (`history_search`, `history_read`, `history_recall`). Своя
      retrieval-схема, НЕ входит в RAG/KB-поиск (инвариант типов уже есть).
- [ ] **запрет изменения/перезаписи историй**: нет update; есть только
      `history_archive` (мягкий архив). Удаление (`history_purge`) — только
      с правами **admin** (пермишен `history:admin:purge`); обычный агент
      получает отказ.
- [ ] прогрессивная суммаризация (L1→L4) и консолидация не «редактируют»
      источники — они их только архивируют/помечают (уже так, закрепить в тестах).

## P5 — Остальное
- [ ] personalization: UserProfile/SeatProfile (procedural-память, doc-ссылки)
- [ ] observability: ActivityRecorder + MemoryMetrics (легковесные)
- [ ] External sync (Obsidian ↔ git-remote) — по возможности
- [ ] **агентская система — УБРАТЬ** (субагенты/многошаговые runs вне скоупа);
      остаётся только **агентский поиск**: клиент сам выполняет multi-hop
      retrieve → read → refine через MCP, а seat-scoped reasoning при
      необходимости идёт через MCP sampling без отдельной модели SLC.

## P6 — Web UI (Rust, standalone)
- [ ] **Web UI портируем на Rust** как отдельный standalone-сервер
      (crate `slc-webui`, консолидированная точка доступа к движку через
      `slc-core`/REST).
- [ ] REST API слой (роуты: docs/history/search/seats/tasks/projects/
      profiles/focus/ideas/reminders/notifications; auth как в MCP).
- [ ] фронтенд (статика + интерактив) обслуживается тем же сервером.
- [ ] деплой: **отдельный Docker** для `slc-webui` (не с MCP-сервером).

## Вне скоупа (другие агенты)
REST/Web UI для Vassista-клиентов (помимо нашего standalone `slc-webui`),
wake/VAD/STT/TTS/LLM, s2s, Metal TTS, e2e talk. Агентская система
(LangGraph-эквивалент, субагенты, runs) — **не портируем**; только агентский
поиск.
