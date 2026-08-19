# Анализ дизайна AI-кодинг продуктов и рекомендации для SLC Dashboard

> Исследование проведено 15.06.2026. Анализ текущих трендов UI/UX в пространстве AI-кодинг инструментов и рекомендации по redesign дашборда SLC.

---

## 1. Карта рынка AI-кодинг продуктов

### 1.1 IDE / Редакторы кода (IDE-first)

| Продукт | Компания | Подход | Дизайн-парадигма |
|---------|----------|--------|------------------|
| **Cursor** | Anysphere | VS Code fork + AI agent | Dark editor, inline chat, sidebar agent |
| **Windsurf** | Codeium (OpenAI) | VS Code fork + Cascade agent | Dark editor, flow-based context, split pane |
| **GitHub Copilot** | GitHub/Microsoft | Plugin для VS Code/JetBrains | Inline suggestions, chat sidebar |

### 1.2 AI App Builders (prompt-to-app)

| Продукт | Компания | Подход | Дизайн-парадигма |
|---------|----------|--------|------------------|
| **Bolt.new** | StackBlitz | WebContainer + AI | Dark chat → live preview, split pane |
| **Lovable** | Lovable | AI full-stack builder | Dark gradient UI, chat + preview |
| **v0** | Vercel | AI UI component generator | Dark minimal, chat → code → preview |
| **Replit** | Replit | Cloud IDE + Agent | Light/dark, chat-driven, integrated deploy |

### 1.3 AI Software Engineers (autonomous agents)

| Продукт | Компания | Подход | Дизайн-парадигма |
|---------|----------|--------|------------------|
| **Devin** | Cognition | Autonomous SWE agent | Light theme, session-based, task board |
| **Claude Code** | Anthropic | Terminal agent | CLI-first, minimal UI |
| **OpenAI Codex** | OpenAI | Cloud agent | Chat → task list → code diff |

---

## 2. Анализ дизайна ключевых продуктов

### 2.1 Cursor (cursor.com)

**Визуальный стиль:**
- **Тема:** Тёмная по умолчанию (VS Code dark+), минималистичная
- **Типографика:** Berkeley Mono (моноширинный) + EB Garamond (заголовки) — уникальный выбор, выделяется на рынке
- **Цвета:** Тёмный фон (#1e1e1e), синий акцент (#007fd4), минимум цветов
- **Макет:** 3-панельный (файловое дерево | редактор | AI chat sidebar)

**Ключевые UI-паттерны:**
- Inline chat: AI-ответы встроены прямо в редактор (не отдельная панель)
- Cmd+K для inline-редактирования кода
- Composer: multi-file editing с диффами в реальном времени
- Tab-completion с ghost-текстом
- Минималистичная верхняя панель без toolbar

**Что можно взять для SLC:**
- Inline-подсказки вместо модальных окон
- Моноширинный шрифт для кодовых блоков
- Ghost-подсказки в полях ввода

---

### 2.2 Windsurf (windsurf.com)

**Визуальный стиль:**
- **Тема:** Тёмная (VS Code dark), более контрастная чем Cursor
- **Типографика:** System UI + JetBrains Mono для кода
- **Цвета:** Фиолетовый акцент (#6366F1), тёмный фон, зелёный для success
- **Макет:** Классический IDE layout с collapsible sidebar

**Ключевые UI-паттерны:**
- Cascade agent: пошаговый визуальный flow выполнения задач
- Flows: визуализация контекста и цепочки рассуждений
- Supercomplete: предиктивные suggestions с анимацией
- Split view: код | preview | chat
- Status bar с индикаторами активности AI

**Что можно взять для SLC:**
- Визуализация AI-процессов (flow/chain of thought)
- Индикаторы статуса агентов в реальном времени
- Фиолетовый как AI-акцентный цвет

---

### 2.3 Bolt.new (bolt.new)

**Визуальный стиль:**
- **Тема:** Тёмная (`data-theme="dark"`) по умолчанию
- **Типографика:** Modern sans-serif, чистый
- **Цвета:** Глубокий чёрный (#0a0a0a), яркий синий/фиолетовый акцент, зелёный для success
- **Макет:** Split pane — chat слева, live preview справа

**Ключевые UI-паттерны:**
- Chat-first interface: всё через промпт
- Live preview с hot-reload
- File tree в sidebar (collapsible)
- Terminal panel внизу
- Deploy button → one-click publish
- "Thinking" индикатор с pulsing animation

**Что можно взять для SLC:**
- Split pane layout: чат/агент | результат
- Pulsing "thinking" индикатор
- One-click deploy/publish паттерн

---

### 2.4 Lovable (lovable.dev)

**Визуальный стиль:**
- **Тема:** Тёмная с градиентами (dark gradient background)
- **Типографика:** Camera Plain (кастомный шрифт), современный sans-serif
- **Цвета:** Фон с мягкими градиентами (pulse.webp background), розовый/фиолетовый акцент
- **Макет:** Центрированный chat → preview pane

**Ключевые UI-паттерны:**
- Animated gradient background (живой, динамичный)
- Chat → code generation → visual preview
- Component-based: AI генерирует React-компоненты
- GitHub integration: auto-commit
- "vibe coding" — минимум UI, максимум промпта
- Glassmorphism элементы (полупрозрачные карточки)

**Что можно взять для SLC:**
- Gradient backgrounds для hero-секций
- Glassmorphism карточки (backdrop-blur)
- Animated background для визуального wow-эффекта

---

### 2.5 v0 (v0.dev → v0.app)

**Визуальный стиль:**
- **Тема:** Тёмная, минималистичная (Vercel-style)
- **Типографика:** Geist Sans + Geist Mono (шрифты Vercel)
- **Цвета:** Чёрный фон (#000), белый текст, синий акцент
- **Макет:** Chat слева → preview/code справа

**Ключевые UI-паттерны:**
- Integrations bar с иконками (10+ сервисов)
- Code/Preview toggle tabs
- Component gallery с превью
- shadcn/ui как базовая библиотека
- Minimal chrome: нет лишних UI-элементов
- Streaming response с typing animation

**Что можно взять для SLC:**
- Geist Mono шрифт (отличный для кода)
- Code/Preview toggle
- Streaming responses с typing animation
- Integration icons bar

---

### 2.6 Replit (replit.com)

**Визуальный стиль:**
- **Тема:** Светлая и тёмная (replit-ui-theme-root)
- **Типографика:** Modern sans-serif,увеличенные шрифты
- **Цвета:** Яркий (#FF6B35 orange accent), чистый белый фон, тёмный sidebar
- **Макет:** IDE layout + chat panel + deploy panel

**Ключевые UI-паттерны:**
- Agent 4: "ideate → design → build" — пошаговый wizard
- Design Mode: визуальный редактор UI
- Inline deploy: кнопка "Deploy" прямо в IDE
- Collaboration: multiplayer cursors
- Progress steps: визуализация этапов выполнения
- Mobile-first подход к сгенерированным приложениям

**Что можно взять для SLC:**
- Пошаговый wizard (ideate → design → build)
- Progress steps для задач агентов
- Inline deploy паттерн
- Collaboration indicators

---

### 2.7 Devin (devin.ai)

**Визуальный стиль:**
- **Тема:** Светлая по умолчанию (theme-light)
- **Типографика:** Modern sans-serif,увеличенный
- **Цвета:** Белый фон, синий акцент (#2563EB), зелёный для success
- **Макет:** Session-based: список сессий → детали сессии

**Ключевые UI-паттерны:**
- Desktop app: "Manage fleets of local and cloud agents from one surface"
- Session list: карточки с превью задач
- Agent status: "running", "completed", "failed" с цветовыми индикаторами
- Task decomposition: AI разбивает задачу на шаги
- Code diff view: inline diff с accept/reject
- Parallel agents: несколько агентов работают одновременно

**Что можно взять для SLC:**
- Session-based UI для управления агентами
- Parallel agent dashboard (несколько агентов одновременно)
- Task decomposition visualization
- Accept/reject для изменений кода

---

### 2.8 OpenAI Codex / ChatGPT

**Визуальный стиль:**
- **Тема:** Тёмная (#343541 фон), минималистичная
- **Типографика:** Söhne (кастомный), system UI fallback
- **Цвета:** Тёмно-серый фон, зелёный акцент (#10A37F), белый текст
- **Макет:** Центрированный chat, task list sidebar

**Ключевые UI-паттерны:**
- Chat-first: всё через диалог
- Task list: Codex создаёт список задач из промпта
- Code diff view: показывает изменения
- Environment indicator: показывает sandbox
- Streaming responses с typing effect
- Minimal UI: нет лишних элементов

**Что можно взять для SLC:**
- Task list из AI-промпта
- Environment/sandbox indicator
- Streaming typing effect

---

## 3. Тренды UI/UX в AI-кодинг пространстве (2025-2026)

### 3.1 Тёмная тема как default
- **90% продуктов** используют тёмную тему по умолчанию
- Корни в IDE-культуре (разработчики предпочитают dark mode)
- Light mode как secondary option
- **Рекомендация для SLC:** Dark-first, light как toggle

### 3.2 Chat-first интерфейс
- **Главный тренд:** Весь interaction через чат/промпт
- Cursor, Bolt, Lovable, v0, Codex — всё начинается с промпта
- Sidebar chat или центрированный chat
- **Рекомендация для SLC:** Центральный чат-интерфейс для взаимодействия с агентами

### 3.3 Split pane layout
- **Стандарт:** Код/чат слева → preview/результат справа
- Cursor: редактор | chat
- Bolt: chat | preview
- v0: chat | code/preview
- **Рекомендация для SLC:** Split layout для Tasks (список | детали)

### 3.4 Streaming & typing animation
- AI-ответы появляются посимвольно (typing effect)
- Streaming responses — стандарт индустрии
- Pulsing indicators во время "thinking"
- **Рекомендация для SLC:** Streaming для логов агентов

### 3.5 Minimal chrome, maximum content
- Убирают лишние toolbar, header, footer
- Content-first: максимум пространства для кода/preview
- FAB (Floating Action Button) вместо toolbar
- **Рекомендация для SLC:** Убрать sidebar, использовать bottom nav или FAB

### 3.6 Status indicators & progress visualization
- Devin: session status (running/completed/failed)
- Windsurf: Cascade flow visualization
- Replit: step-by-step progress
- **Рекомендация для SLC:** Визуализация статуса задач и агентов

### 3.7 Glassmorphism & gradients
- Lovable: animated gradient backgrounds
- v0: glassmorphism cards
- Soft shadows, backdrop-blur
- **Рекомендация для SLC:** Использовать для hero-секций и карточек

### 3.8 AI-native color palette
- Фиолетовый = AI/ML (Windsurf, Lovable)
- Зелёный = success/running
- Синий = primary action
- Красный = error/failed
- **Рекомендация для SLC:** Добавить фиолетовый как AI-акцент

---

## 4. Рекомендации по redesign SLC Dashboard

### 4.1 Текущее состояние SLC
- **Страницы:** Dashboard, Tasks, Seats, Agents, KnowledgeBase, Admin, Login, Activity
- **UI:** Light theme, Tailwind CSS, стандартные карточки
- **Проблемы:** Нет AI-native look, нет streaming, нет status visualization

### 4.2 Предлагаемый дизайн-Direction

#### Стиль: "AI-Native Dark Dashboard"
- **Вдохновение:** Cursor + Devin + v0
- **Тема:** Dark-first (bg: #0a0a0a или #111111)
- **Акценты:** Фиолетовый (#8B5CF6) для AI, синий (#3B82F6) для actions
- **Типографика:** Geist Sans (UI) + Geist Mono (code)

#### Layout: Sidebar + Content + Detail Panel
```
┌─────────────────────────────────────────────────┐
│  Logo    Dashboard  Tasks  Agents  KB    [User] │
├────────┬────────────────────────┬───────────────┤
│        │                        │               │
│ Nav    │  Main Content          │  Detail Panel │
│ (icon  │  (cards, lists,        │  (contextual) │
│  bar)  │   charts)              │               │
│        │                        │               │
├────────┴────────────────────────┴───────────────┤
│  Agent Status Bar: [Agent1: running] [Agent2: idle] │
└─────────────────────────────────────────────────┘
```

### 4.3 Конкретные рекомендации по страницам

#### Dashboard (главная)
- **Сейчас:** Набор карточек с метриками
- **Предложение:**
  - Agent Activity Feed (streaming, real-time)
  - Quick Actions: "Create Task", "Start Agent", "Upload Knowledge"
  - System Health с pulsing indicators
  - Recent Tasks с status badges
  - **Референс:** Devin session list + Replit dashboard

#### Tasks
- **Сейчас:** Таблица + фильтры
- **Предложение:**
  - Kanban board (Backlog → Active → Done)
  - Task cards с agent avatar, progress bar, priority badge
  - Split view: список слева → детали справа
  - **Референс:** Linear.app task board + Cursor composer view

#### Agents
- **Сейчас:** Список агентов с формой создания
- **Предложение:**
  - Agent cards с live status (running/idle/error)
  - Parallel agent visualization (как Devin Desktop)
  - Agent creation wizard (как Replit Agent 4)
  - **Референс:** Devin Desktop + Windsurf Cascade

#### Knowledge Base
- **Сейчас:** Список документов + редактор
- **Предложение:**
  - Graph view (как KnowledgeGraph, но как основной вид)
  - Search-first interface
  - Document cards с preview
  - **Референс:** Notion + Obsidian graph view

#### Admin
- **Сейчас:** Tabs с настройками
- **Предложение:**
  - System dashboard с real-time metrics
  - User management с activity timeline
  - Config editor с live validation
  - **Референс:** Vercel dashboard + Railway dashboard

---

## 5. Конкретные референсы (ссылки)

### 5.1 Layout & Structure
| Элемент | Референс | URL |
|---------|----------|-----|
| Sidebar navigation | Linear.app | https://linear.app |
| Split pane layout | Cursor | https://cursor.com |
| Tab navigation | Vercel Dashboard | https://vercel.com/dashboard |
| Command palette | Cursor (Cmd+K) | https://cursor.com |

### 5.2 AI-специфичные UI
| Элемент | Референс | URL |
|---------|----------|-----|
| Agent status indicators | Devin Desktop | https://devin.ai/desktop |
| Streaming chat | ChatGPT | https://chatgpt.com |
| Task decomposition | OpenAI Codex | https://chatgpt.com/codex |
| Cascade/flow visualization | Windsurf | https://windsurf.com |
| Thinking indicator | Bolt.new | https://bolt.new |

### 5.3 Visual Style
| Элемент | Референс | URL |
|---------|----------|-----|
| Dark theme + gradients | Lovable | https://lovable.dev |
| Minimal dark UI | v0 | https://v0.dev |
| Geist typography | Vercel | https://vercel.com |
| Glassmorphism cards | Lovable | https://lovable.dev |
| Status badges | Linear.app | https://linear.app |

### 5.4 Component Libraries (для реализации)
| Библиотека | Описание | URL |
|------------|----------|-----|
| shadcn/ui | Компоненты на Radix + Tailwind | https://ui.shadcn.com |
| Radix Primitives | Accessible UI primitives | https://radix-ui.com |
| Tremor | Dashboard charts & components | https://tremor.so |
| Magic UI | Animated components | https://magicui.design |
| Aceternity UI | Animated dark components | https://ui.aceternity.com |

---

## 6. План действий

### Phase 1: Foundation (1-2 дня)
1. Добавить dark theme support (toggle light/dark)
2. Обновить цветовую палитру: добавить фиолетовый AI-акцент
3. Заменить шрифт на Geist Sans/Mono
4. Обновить CSS variables для dark theme

### Phase 2: Dashboard (2-3 дня)
1. Redesign Dashboard: agent activity feed + quick actions
2. Добавить streaming indicators
3. Обновить карточки: glassmorphism + status badges
4. Agent status bar внизу

### Phase 3: Tasks & Agents (2-3 дня)
1. Tasks: Kanban view + split pane
2. Agents: live status + creation wizard
3. Agent cards с progress visualization

### Phase 4: Polish (1-2 дня)
1. Анимации (Framer Motion или Svelte transitions)
2. Command palette (Cmd+K)
3. Responsive design
4. Accessibility

---

## 7. Технические рекомендации

### CSS Architecture
- Использовать CSS variables для theme switching
- Tailwind dark mode: `class` strategy (уже настроено)
- Компоненты из `src/components/ui/` — база для нового дизайна

### Цветовая палитра (dark theme)
```css
--bg-primary: #0a0a0a;      /* основной фон */
--bg-secondary: #111111;     /* карточки, sidebar */
--bg-tertiary: #1a1a1a;      /* hover, active */
--text-primary: #fafafa;     /* основной текст */
--text-secondary: #a1a1aa;   /* вторичный текст */
--accent-ai: #8B5CF6;        /* AI-элементы (фиолетовый) */
--accent-primary: #3B82F6;   /* actions (синий) */
--accent-success: #22C55E;   /* success (зелёный) */
--accent-error: #EF4444;     /* error (красный) */
--accent-warning: #F59E0B;   /* warning (жёлтый) */
```

### Типографика
```css
--font-sans: 'Geist Sans', system-ui, sans-serif;
--font-mono: 'Geist Mono', monospace;
```

---

## 8. Выводы

1. **Dark theme — стандарт индустрии.** Все крупные AI-кодинг продукты используют тёмную тему.
2. **Chat-first — главный тренд.** Взаимодействие через промпт/чат已成为标准。
3. **Split pane — оптимальный layout.** Контент + контекст рядом.
4. **Streaming — обязательно.** AI-ответы должны появляться в реальном времени.
5. **Minimal UI — less is more.** Убрать лишний chrome, максимум контента.
6. **AI-специфичные цвета.** Фиолетовый = AI, зелёный = success, синий = action.
7. **Status visualization.** Пользователь должен видеть, что делает AI в реальном времени.

SLC имеет хорошую базу (Tailwind, компоненты, dark mode support). Основная работа — визуальный redesign + добавление AI-native паттернов (streaming, status indicators, split pane).
