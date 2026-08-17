# План визуального redesign SLC Web UI

> Основан на `DESIGN_RESEARCH.md` (15.06.2026).  
> Текущая ветка: `ui-refactor`.  
> Предварительные шаги (UI-компоненты, CSS) выполнены в `PLAN_REFACTOR.md`.

---

## Цель

Привести SLC Dashboard к AI-Native dark дизайну в стиле Cursor / Devin / v0.  
Обновить палитру, типографику, layout ключевых страниц, добавить AI-специфичные паттерны (status indicators, streaming look, split pane).

---

## 0. Исходное состояние

| Что | Сейчас | Источник |
|-----|--------|----------|
| Тема | Dark toggle через `localStorage` + `.dark` class | `src/lib/Layout.svelte:31-41` |
| Шрифт | Inter | `src/app.css:8` |
| Палитра | Tailwind defaults (blue-600, gray-200 и т.д.) | `tailwind.config.*`, inline |
| Layout | Top nav + full-width content | `src/lib/Layout.svelte` |
| Страницы | Dashboard, Tasks, Seats, Agents, Agent, KnowledgeBase, Activity, Admin, Login | `src/Router.svelte:16-27` |
| UI-компоненты | `src/components/ui/` — Modal, Alert, Spinner, Card, Button, Badge, ProgressBar, StatsGrid, StatCard, FilterPanel, DataList | Шаг 1 `PLAN_REFACTOR.md` |

---

## Phase 1 — Foundation: dark theme, палитра, типографика

**Цель:** Заложить визуальную базу, чтобы все последующие страницы сразу выглядели AI-native.

### 1.1 Обновить палитру CSS variables

**Файл:** `src/app.css`

Добавить CSS custom properties (dark-first):

```css
:root {
  /* Dark theme (default) */
  --bg-primary: #09090b;
  --bg-secondary: #111113;
  --bg-tertiary: #18181b;
  --bg-elevated: #1e1e21;
  --border-subtle: #27272a;
  --border-default: #3f3f46;

  --text-primary: #fafafa;
  --text-secondary: #a1a1aa;
  --text-tertiary: #71717a;

  --accent-ai: #8b5cf6;         /* фиолетовый — AI-элементы */
  --accent-ai-hover: #a78bfa;
  --accent-primary: #3b82f6;    /* синий — primary actions */
  --accent-success: #22c55e;
  --accent-error: #ef4444;
  --accent-warning: #eab308;
}

.light {
  --bg-primary: #fafafa;
  --bg-secondary: #ffffff;
  --bg-tertiary: #f4f4f5;
  --bg-elevated: #ffffff;
  --border-subtle: #e4e4e7;
  --border-default: #d4d4d8;

  --text-primary: #09090b;
  --text-secondary: #71717a;
  --text-tertiary: #a1a1aa;
}
```

Обновить `body` стили: `background-color: var(--bg-primary); color: var(--text-primary);`

**Файл:** `tailwind.config.*`

Добавить кастомные цвета в `theme.extend.colors`:
```js
colors: {
  ai: { DEFAULT: '#8b5cf6', hover: '#a78bfa' },
  surface: { 1: 'var(--bg-primary)', 2: 'var(--bg-secondary)', 3: 'var(--bg-tertiary)', elevated: 'var(--bg-elevated)' },
  subtle: 'var(--border-subtle)',
}
```

**Файлы:** `src/styles/components.css` — обновить card, modal, alert, btn классы на CSS variables.

**Зависимости:** нет  
**Оценка:** 2–3 ч

---

### 1.2 Заменить шрифт на Geist

**Файл:** `index.html` — добавить `<link>` для Geist Sans + Geist Mono (Google Fonts или self-hosted через `public/fonts/`).

**Файл:** `src/app.css`
```css
:root {
  font-family: 'Geist Sans', system-ui, -apple-system, sans-serif;
}
code, pre, .font-mono {
  font-family: 'Geist Mono', ui-monospace, monospace;
}
```

**Альтернатива:** если self-hosted — скачать woff2, добавить `@font-face` в `src/styles/fonts.css`.

**Зависимости:** нет  
**Оценка:** 1 ч

---

### 1.3 Обновить Layout — навигация

**Файл:** `src/lib/Layout.svelte`

Текущий layout: top nav с emoji-иконками, full-width content.

Новый layout — **icon sidebar + top bar** (вдохновение Cursor / Linear):

```
┌──────────────────────────────────────────┐
│ [≡] SLC Control Panel        [🌙] [👤]  │  ← top bar (thin)
├────┬─────────────────────────────────────┤
│ 📊 │                                     │
│ 📚 │         Page Content                │  ← main area
│ 📡 │                                     │
│ 📋 │                                     │
│ 🧠 │                                     │
│ 🤖 │                                     │
│ 📈 │                                     │
│ ⚙️ │                                     │
└────┴─────────────────────────────────────┘
```

- Sidebar: 56px, иконки (сейчас emoji → позже SVG), тёмный фон `var(--bg-secondary)`
- Активный пункт: accent-ai подсветка слева
- Hover: subtle highlight
- Top bar: breadcrumbs + theme toggle + user avatar
- Контент: `margin-left: 56px`, padding 24px

**Зависимости:** 1.1 (цвета)  
**Оценка:** 3–4 ч

---

## Phase 2 — Dashboard (главная страница)

**Цель:** Переделать Dashboard из набора карточек в AI-native dashboard с activity feed и quick actions.

**Файл:** `src/pages/Dashboard.svelte` (176 строк → ~250)

### 2.1 Структура нового Dashboard

```
┌─────────────────────────────────────────────────┐
│  Dashboard                                      │
│                                                 │
│  ┌─ Quick Actions ─────────────────────────────┐│
│  │ [+ New Task]  [+ Start Agent]  [⬆ Upload]  ││
│  └─────────────────────────────────────────────┘│
│                                                 │
│  ┌─ Stats Row ────────────────────────────────┐│
│  │ [Active Tasks] [Online Seats] [KB Docs]    ││
│  │ [Agent Status]                              ││
│  └─────────────────────────────────────────────┘│
│                                                 │
│  ┌─ Agent Activity ──┐ ┌─ Recent Tasks ───────┐│
│  │ streaming log      │ │ task1 ● active       ││
│  │ agent1: running    │ │ task2 ○ pending      ││
│  │ agent2: idle       │ │ task3 ● completed    ││
│  │ ...                │ │ ...                  ││
│  └────────────────────┘ └──────────────────────┘│
└─────────────────────────────────────────────────┘
```

### 2.2 Компоненты

| Блок | Что делать | Файл |
|------|-----------|------|
| Quick Actions | Новый компонент `QuickActions.svelte` — 3 кнопки variant=primary/secondary, router navigate | `src/components/dashboard/QuickActions.svelte` (новый) |
| Stats Row | Использовать `<StatsGrid>` + `<StatCard>` из ui/ (уже есть) | `src/pages/Dashboard.svelte` |
| Agent Activity Feed | Новый компонент, polling `/api/admin/stats` + `/api/seats/overview`, анимация «typing» для новых событий | `src/components/dashboard/ActivityFeed.svelte` (новый) |
| Recent Tasks | Существующий `TasksList` (или сокращённая версия) | `src/pages/Dashboard.svelte` |

**Зависимости:** Phase 1  
**Оценка:** 4–5 ч

---

## Phase 3 — Tasks: split pane + Kanban

**Цель:** Добавить split-pane layout (список слева → детали справа) и опциональный Kanban-вид.

**Файлы:** `src/pages/Tasks.svelte` (253 строки), `src/components/tasks/TaskDetails.svelte`

### 3.1 Split Pane Layout

```
┌─────────────────────────────────────────────────┐
│ Tasks           [🔍 Search] [Filters ▼] [≡ ⊞]  │
├─────────────────────┬───────────────────────────┤
│                     │                           │
│  Task List          │  Task Details / Edit      │
│  (scrollable)       │  (или пустое состояние    │
│                     │   "Select a task")        │
│  ┌───────────────┐  │                           │
│  │ ● Task 1      │  │                           │
│  │ ○ Task 2      │  │                           │
│  │ ● Task 3      │  │                           │
│  └───────────────┘  │                           │
│                     │                           │
└─────────────────────┴───────────────────────────┘
```

- Левая панель: 320px fixed, скролл
- Правая панель: flex-1
- `selectedTask` state → показывает детали
- Пустое состояние: "Select a task to view details" с иконкой
- Toggle: list view ↔ kanban view (переключатель в toolbar)

### 3.2 Kanban (опционально)

3 колонки: `Backlog` | `In Progress` | `Completed`.  
Drag-and-drop — Svelte action или `svelte-dnd-action`.

**Зависимости:** Phase 1  
**Оценка:** 4–5 ч (split pane 2–3 ч, kanban 2–3 ч)

---

## Phase 4 — Agents: live status + wizard

**Цель:** Обновить Agents page: карточки с live status, creation wizard, параллельные агенты.

**Файлы:** `src/pages/Agents.svelte` (796 строк), `src/pages/Agent.svelte` (67 строк)

### 4.1 Agent Cards с Live Status

```
┌─────────────────────────────────────────────────┐
│ Agents                    [+ Create Agent]      │
├─────────────────────────────────────────────────┤
│                                                 │
│  ┌─ Agent Card ────────────────────────────────┐│
│  │ 🤖 My Agent                    ● Running    ││
│  │ GPT-4 | created 2d ago                      ││
│  │ ████████░░ 80% (last task)                  ││
│  │ [View] [Stop] [Delete]                      ││
│  └─────────────────────────────────────────────┘│
│                                                 │
│  ┌─ Agent Card ────────────────────────────────┐│
│  │ 🤖 Test Agent                   ○ Idle      ││
│  │ Claude 3.5 | created 5d ago                 ││
│  │                                             ││
│  │ [View] [Run] [Delete]                       ││
│  └─────────────────────────────────────────────┘│
│                                                 │
└─────────────────────────────────────────────────┘
```

- Status badge: pulsing green dot для Running, gray для Idle, red для Error
- ProgressBar (из ui/) для текущей задачи
- Card layout (grid 1-2-3 колонки responsive)

### 4.2 Creation Wizard

3-шаговый wizard вместо модалки:
1. **Name & Description** — text inputs
2. **Model & Config** — select model, system prompt
3. **Review & Create** — summary + кнопка

Компонент: `src/components/agents/AgentWizard.svelte` (новый)

**Зависимости:** Phase 1  
**Оценка:** 4–5 ч

---

## Phase 5 — Knowledge Base: search-first + graph

**Цель:** Сделать поиск главной точкой входа, graph view как основной вид.

**Файл:** `src/pages/KnowledgeBase.svelte` (489 строк)

### 5.1 Search-First Layout

```
┌─────────────────────────────────────────────────┐
│  [🔍 Search knowledge base...]                  │
├──────────────┬──────────────────────────────────┤
│  Categories  │  Results / Documents             │
│  ─────────── │                                  │
│  📄 Docs (5) │  ┌─ Doc Card ──────────────────┐ │
│  🧩 Modules  │  │ Title                       │ │
│  📦 Config   │  │ preview text...  [tags]     │ │
│              │  │ ● Indexed  |  3 references  │ │
│              │  └─────────────────────────────┘ │
│              │                                  │
│  [Graph View]│                                  │
└──────────────┴──────────────────────────────────┘
```

- Search bar сверху (full width, крупный, с glassmorphism)
- Sidebar: категории + graph toggle
- Default view: список документов с preview
- Graph view: существующий `KnowledgeGraph.svelte` по клику

**Зависимости:** Phase 1  
**Оценка:** 3–4 ч

---

## Phase 6 — Seats: status grid + charts

**Файл:** `src/pages/Seats.svelte` (392 строк)

### 6.1 Обновлённый Seats page

- Stats row: Active seats, Total requests, Tokens used (через `<StatsGrid>`)
- Seat cards: grid layout, status badge (active/expired), sparkline для usage
- Seat details: split pane (список | детали) как в Tasks
- Charts: существующий `SeatCharts.svelte` в detail panel

**Зависимости:** Phase 1  
**Оценка:** 3–4 ч

---

## Phase 7 — Admin: dashboard-style

**Файл:** `src/pages/Admin.svelte` (176 строк)

### 7.1 Admin Dashboard

- Tab navigation → sidebar sub-nav (как Cursor settings)
- System Health: real-time metrics с pulsing indicators
- User Management: table + inline edit (без модалок где возможно)
- Config: Monaco editor (или CodeMirror) вместо textarea

**Зависимости:** Phase 1  
**Оценка:** 3–4 ч

---

## Phase 8 — Login: modern dark

**Файл:** `src/pages/Login.svelte` (104 строки)

### 8.1 Redesign Login

- Центрированная карточка на тёмном фоне
- Glassmorphism card (`backdrop-blur`, `bg-white/5`)
- Logo + gradient glow background
- Minimal: только username + password + кнопка
- Анимация: fade-in + subtle scale

**Зависимости:** Phase 1  
**Оценка:** 1–2 ч

---

## Phase 9 — Анимации и полировка

### 9.1 Page Transitions
- Svelte `transition:fade` или `transition:fly` при смене страниц
- Файл: `src/Router.svelte`

### 9.2 Micro-interactions
- Hover: subtle scale на карточках (`transform: scale(1.01)`)
- Click: ripple или press effect на кнопках
- Loading: skeleton screens вместо пустых мест

### 9.3 Command Palette (Cmd+K)
- Новый компонент `src/components/ui/CommandPalette.svelte`
- Поиск по страницам, задачам, документам
- Keyboard navigation
- **Оценка:** 3–4 ч (отдельная фича)

---

## Phase 10 — Проверка и деплой

1. `npm run build` — нет ошибок
2. `npm run check` — svelte-check чист
3. Docker build + deploy на localhost:2002
4. Ручная проверка всех страниц в dark и light теме
5. Responsive: проверить на 1024px и 1440px

---

## Сводная таблица

| Phase | Описание | Файлов | Оценка |
|-------|----------|--------|--------|
| 1. Foundation | Палитра, шрифт, CSS variables, tailwind config | ~5 | 3–4 ч |
| 2. Dashboard | Quick actions, stats, activity feed | ~4 | 4–5 ч |
| 3. Tasks | Split pane, kanban | ~3 | 4–5 ч |
| 4. Agents | Live status, creation wizard | ~4 | 4–5 ч |
| 5. Knowledge Base | Search-first, graph toggle | ~2 | 3–4 ч |
| 6. Seats | Status grid, split pane | ~2 | 3–4 ч |
| 7. Admin | Dashboard-style tabs | ~2 | 3–4 ч |
| 8. Login | Glassmorphism card | 1 | 1–2 ч |
| 9. Polish | Animations, command palette | ~4 | 5–7 ч |
| 10. Проверка | Build, test, deploy | — | 2 ч |
| **Итого** | | **~27 файлов** | **~32–43 ч** |

---

## Зависимости между фазами

```
Phase 1 (Foundation)
 ├── Phase 2 (Dashboard)
 ├── Phase 3 (Tasks)
 ├── Phase 4 (Agents)
 ├── Phase 5 (KB)
 ├── Phase 6 (Seats)
 ├── Phase 7 (Admin)
 └── Phase 8 (Login)
      └── Phase 9 (Polish)
           └── Phase 10 (Проверка)
```

Phases 2–8 **независимы** друг от друга и могут выполняться в любом порядке после Phase 1.  
Phase 9 (Polish) — после всех страниц.  
Phase 10 — финальная проверка.

---

## Риски

- **Шрифты Geist:** могут быть недоступны через Google Fonts. Решение: self-hosted woff2 через `public/fonts/`.
- **CSS variables + Tailwind:** `@apply` не работает с CSS variables напрямую. Решение: использовать `var()` в обычных CSS rules, `@apply` — для статических значений.
- **Breaking changes:** обновление палитры затронет все страницы. Решение: phase 1 делать отдельным коммитом, проверять каждую страницу.
- **Split pane на мобильных:** не работает на <768px. Решение: fallback на stacked layout.

---

## Что НЕ входит

- Изменение API-логики
- Изменение маршрутизации (hash-based остаётся)
- Добавление новых страниц
- Рефакторинг `src/lib/*`
- Замена Chart.js/D3
