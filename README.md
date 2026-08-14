# SLC MCP

SLC (Smart Layered Context) — движок памяти для кодинг-агентов: единая база
документов (проекты, задачи, скилы, знания), эпизодическая история с
прогрессивной суммаризацией, гибридный поиск (семантика + BM25), фокусы,
напоминания. Работает как MCP-сервер (`tools` + `prompts`) и CLI.

## Развертывание: `slc-mcp init`

Модель эмбеддингов **никогда не скачивается автоматически** на первый
запрос — её готовит консольный визард:

```bash
slc-mcp init
```

Визард спрашивает:

1. **Провайдер эмбеддингов**:
   - `candle` — встроенный инференс (модель качается на эту машину);
   - `ollama` / `lmstudio` — внешний GPU-сервер;
   - `hash` — CPU-эмбеддинги без моделей (слабые машины, виртуалки).
2. **Устройство** для candle (`auto` | `cuda` | `metal` | `cpu`; визард
   показывает, что обнаружено).
3. **Модель**: по умолчанию `BAAI/bge-m3` (1024-мер, мультиязычная, RU) на
   GPU и `intfloat/multilingual-e5-small` (384-мер) на CPU; можно указать
   любой HF repo id.

Затем визард **сразу скачивает модель** в кэш Hugging Face
(`~/.cache/huggingface`), записывает `.env` (`SLC_LLM`, `SLC_EMBED_DEVICE`,
`SLC_EMBED_MODEL`) и делает контрольный эмбеддинг. Неинтерактивно:

```bash
slc-mcp init --llm candle --device cpu --model intfloat/multilingual-e5-small
```

### Выбор провайдера по умолчанию

- Явный `SLC_LLM` (`hash` | `ollama` | `lmstudio` | `candle`) — всегда его;
  если он отвалился, поиск честно деградирует в text-only (без тихого
  переключения).
- Ничего не задано: GPU есть и модель скачана → candle; иначе → hash.

## Запуск

```bash
slc-mcp serve --port 3000          # MCP: POST /mcp, SSE /sse, /health
# хранилище: Obsidian vault (default), --sqlite, --mongodb
```

## Управление CLI

| Команда | Что делает |
|---|---|
| `slc-mcp init` | Визард развертывания: провайдер, модель, скачивание, `.env` |
| `slc-mcp reindex-embeddings [--seat X]` | Пересобрать эмбеддинги после смены модели/настроек |
| `slc-mcp search "<query>" [--seat X]` | Гибридный поиск |
| `slc-mcp status` | Бэкенд и путь хранилища |
| `slc-mcp remember/compress/consolidate` | Эпизодика и суммаризация |
| `slc-mcp migrate --from <legacy>` | Миграция легаси-ваулта (id → слаги, папки) |

Сменил модель в `init`? Пересобери эмбеддинги:

```bash
slc-mcp reindex-embeddings
```

Поиск сам отфильтровывает записи от другой модели (по размерности) и при
первом поиске лениво пересобирает устаревшие, так что даже без `reindex`
ничего не сломается.

## Инференс

Каскад (в порядке приоритета): **внешний GPU-сервер → встроенный candle →
CPU-hash**. Подробности в `crates/slc-core/src/candle_emb.rs`:

- `SLC_LLM=hash` — детерминированные hash-эмбеддинги, без моделей;
- `SLC_LLM=candle` — встроенный инференс (candle): GPU → `bge-m3`, CPU →
  `e5-small`; модель грузится из кэша (автоскачивания нет);
- `SLC_LLM=ollama` / `lmstudio` — внешние серверы (reasoning + embeddings);
- `SLC_MCP_SAMPLING=true` — инференс через MCP-клиента (sampling), эмбеддинги
  недоступны → text-only поиск.

GPU-бэкенды: **CUDA** (Linux/Windows, собери с `--features slc-core/cuda`,
нужен CUDA toolkit) → `bge-m3` на GPU; на **macOS** candle работает на
Accelerate-ускоренном CPU (Metal-бэкенд candle не поддерживает layer-norm —
проверено e2e), поэтому `init` предложит `e5-small`; CPU-машины без моделей
→ hash.

## Хуки для харнеса

Готовый hook-скрипт, который обвязка дёргает в конце каждого агент-лупа и
который автоматически сохраняет контекст в SLC — см. [`hooks/README.md`](hooks/README.md).
