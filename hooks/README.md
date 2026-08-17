# SLC hooks — авто-сохранение контекста в конце агент-лупа

`slc-hook.py` — готовый hook для **обвязки кодинг-агента**: харнес вызывает
его в конце каждого цикла (агент закончил шаг/сессию), а он автоматически
сохраняет контекст сита в SLC через MCP-тул `update_context`:

- `save <summary>` — снимок контекста в историю (как `/save_context`);
- `update` — просто пересобрать текущий срез (как `/update_context`);
- саммари по умолчанию собирается из последнего git-коммита
  (`git log -1` + `diff --stat`), можно передать своё или `-` из stdin.

Требования: `python3` (только стандартная библиотека), запущенный
`slc-mcp serve`. Скрипт не зависит от LLM-сервера — работает и с hash-режимом
(`SLC_LLM=hash`).

## Быстрая проверка

```bash
# сервер: slc-mcp serve --port 3000
export SLC_SEAT_ID=dev
python3 hooks/slc-hook.py save "проверка хука"
```

Ответ содержит `limit_chars/used_chars/compressed` — если контекст сжат,
появится предупреждение.

## Подключение к харнесу

### Claude Code

`~/.claude/settings.json` (или `.claude/settings.json` проекта):

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

- `Stop` — конец каждого ответа агента в сессии; снимок уходит в историю.
- Если хочется реже (только конец крупных этапов) — используйте
  `SubagentStop` или вызывайте вручную через `/save_context`.

### ZCode (клиент, в котором ведётся разработка)

Хуки ZCode конфигурируются в клиенте (см. справку клиента по hooks);
команда для события конца сессии/цикла та же:

```
SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
```

### Любой агент / обёртка

Обёртка в zsh/bash вокруг вашего агента — работает везде, где нет системы
хуков:

```bash
agent() {
  "$@"                                   # запуск агента
  local rc=$?
  SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
  return $rc
}
# alias claude="agent claude"  и т.п.
```

### Минимальный вариант — git pre-commit

```bash
# .git/hooks/pre-commit
SLC_SEAT_ID=dev python3 /path/to/slc-hook.py save
```

## Окружение

| Переменная       | Значение по умолчанию        | Описание                          |
|------------------|------------------------------|-----------------------------------|
| `SLC_MCP_URL`    | `http://127.0.0.1:3000/mcp`  | адрес MCP-сервера                 |
| `SLC_SEAT_ID`    | — (обязательна)              | сид; можно `--seat`               |
| `SLC_MCP_AUTH`   | `legacy_seat_id`             | `bearer_plus_seat` — если включён |
| `SLC_MCP_TOKEN`  | —                            | токен для `bearer_plus_seat`      |
| `SLC_SKIP`       | пусто                        | любое непустое значение — выйти 0 |

Разные проекты = разные сиды: для каждого проекта/репозитория задавайте
свой `SLC_SEAT_ID` (например, в `.env` проекта или в команде хука).
