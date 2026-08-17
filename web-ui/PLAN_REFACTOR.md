# План рефакторинга компонентов SLC Web UI

## Цель

Вынести повторяющиеся UI-паттерны в переиспользуемые компоненты и собрать весь CSS в отдельный файл стилей. Устранить копипасту между страницами.

---

## 1. Анализ дублирования

### 1.1 Модальные окна (9 мест)
Одинаковая обёртка `fixed inset-0 bg-black/50 flex items-center justify-center z-50` + `bg-white dark:bg-gray-800 rounded-lg p-6 max-w-... w-full mx-4` встречается в:
- `CreateTaskModal.svelte`
- `CreateSeatModal.svelte`
- `ImportModal.svelte`
- `TaskDetails.svelte`
- `UserManagement.svelte` (Create User Modal, Edit User Modal)
- `GroupsManagement.svelte` (Create Group Modal, Edit Group Modal)
- `KnowledgeGraph.svelte` (Document modal)
- `KnowledgeBase.svelte` (fullscreen wrapper для DocumentEditor)

### 1.2 Alert/Toast сообщения (25+ мест)
Каждый компонент дублирует блоки ошибок/успеха:
- `bg-red-100 border border-red-400 text-red-700 px-4 py-3 rounded`
- `bg-green-100 border border-green-400 text-green-700 px-4 py-3 rounded`
- `bg-red-50 dark:bg-red-900/20 text-red-600 dark:text-red-400 p-3 rounded-lg text-sm`
- `bg-green-50 dark:bg-green-900/20 text-green-600 dark:text-green-400 p-3 rounded-lg text-sm`

Затронутые файлы: `UserManagement`, `GroupsManagement`, `LogsViewer`, `BackupManager`, `CleanupPanel`, `ResponseLimitConfig`, `SearchPipelineConfig`, `OAuthAccess`, `ResourceMonitor`, `DocumentEditor`, `KnowledgeBase`, `Tasks`, `Seats`, `Agent`, `Admin`.

### 1.3 Spinner загрузки (15+ мест)
Повторяется `animate-spin w-6 h-6 border-2 border-blue-600 border-t-transparent rounded-full` в:
`EmbeddingsPanel`, `DocumentList`, `SeatsList`, `ConfigEditor`, `GroupsManagement`, `ResourceMonitor`, `UserManagement`, `SearchPipelineConfig`, `Tasks`, `Seats`.

### 1.4 Карточки-контейнеры (20+ мест)
`bg-white dark:bg-gray-800 rounded-lg shadow p-6` / `rounded-lg border` в:
`SystemHealth`, `ConfigEditor`, `ResourceMonitor`, `EmbeddingsPanel`, `BackupManager`, `CleanupPanel`, `ResponseLimitConfig`, `AnalyticsCharts`, `LogsViewer`, `GroupsManagement`, `DocumentList`, `DocumentViewer`, `DocumentEditor`, `SearchBar`, `KnowledgeGraph`, `SeatCharts`, `SeatDetails`, `TasksList`, `SeatsList`.

### 1.5 Кнопки (30+ мест)
Повторяются варианты:
- primary: `px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white rounded-lg`
- secondary: `px-4 py-2 bg-gray-200 ... rounded-lg`
- danger: `px-4 py-2 bg-red-600 ... rounded-lg`
- success: `px-4 py-2 bg-green-600 ... rounded-lg`

### 1.6 Бейджи (10+ мест)
`px-2 py-0.5 text-xs rounded-full bg-{color}-100 text-{color}-700 dark:bg-{color}-900/30 dark:text-{color}-300` в:
`SeatDetails`, `TasksList`, `SeatCard`, `KnowledgeBase`.

### 1.7 Progress bar (3 места)
`w-full h-2 bg-gray-200 dark:bg-gray-700 rounded-full` + внутренний `h-full bg-blue-600 rounded-full` в:
`SeatCharts`, `EmbeddingsPanel`, `ResourceMonitor`.

### 1.8 Filter panels (2 места)
`TaskFilters.svelte` и `SeatFilters.svelte` — идентичная структура: заголовок "Filters", поля поиска, селекты, кнопка Clear.

### 1.9 Lists с loading/empty states (3 места)
`DocumentList.svelte`, `SeatsList.svelte`, `TasksList.svelte` — одинаковая логика: показать спиннер → показать "No items" → показать список.

### 1.10 Inline `<style>` (1 место)
`ActivityTimeline.svelte` содержит собственный `<style>` с анимацией `fadeIn`, которую можно вынести в общий CSS.

---

## 2. Архитектура решения

### 2.1 CSS-файл
Создать `src/styles/components.css` и импортировать его в `src/app.css`.
В нём разместить классы для всех повторяющихся паттернов:
- `.modal-overlay`, `.modal-panel`
- `.alert`, `.alert--error`, `.alert--success`, `.alert--info`, `.alert--warning`
- `.spinner`, `.spinner--sm`, `.spinner--md`, `.spinner--lg`
- `.card`, `.card--padded`, `.card__header`, `.card__title`
- `.btn`, `.btn--primary`, `.btn--secondary`, `.btn--danger`, `.btn--success`, `.btn--ghost`
- `.form-label`, `.form-input`, `.form-select`, `.form-textarea`
- `.badge`, `.badge--green`, `.badge--red`, `.badge--blue`, `.badge--yellow`, `.badge--gray`, `.badge--purple`
- `.progress-bar`, `.progress-bar__fill`
- `.stats-grid`, `.stat-card`, `.stat-card__label`, `.stat-card__value`
- `.filter-panel`, `.filter-panel__title`
- `.data-list`, `.data-list__header`, `.data-list__body`, `.data-list__empty`, `.data-list__loading`
- `.data-table`, `.data-table th`, `.data-table td`
- `.animate-fade-in`

### 2.2 UI-компоненты
Создать `src/components/ui/` с компонентами:
- `Modal.svelte` — обёртка для модальных окон (overlay + panel)
- `Alert.svelte` — уведомления (error/success/info/warning)
- `Spinner.svelte` — индикатор загрузки
- `Card.svelte` — карточка-контейнер
- `Button.svelte` — кнопка с вариантами
- `Badge.svelte` — бейдж-метка
- `ProgressBar.svelte` — полоса прогресса
- `StatsGrid.svelte` + `StatCard.svelte` — сетка метрик
- `FilterPanel.svelte` — панель фильтров
- `DataList.svelte` — список с loading/empty состояниями

Все компоненты используют Svelte 5 runes (`$props`, `$state`, `$derived`).

### 2.3 Принципы
UI-компоненты используют CSS-классы из `components.css`. Страничные компоненты используют UI-компоненты через импорт, а не копируют class-строки.

Tailwind остаётся на месте: `components.css` добавляет именованные utility-классы поверх Tailwind, не заменяя его.

---

## 3. Порядок рефакторинга

### ✅ Шаг 1. CSS и UI-база
1. Создать `src/styles/components.css` со всеми классами из раздела 2.1.
2. Добавить `@import './styles/components.css';` в `src/app.css` (**до** `@tailwind` директив).
3. Создать `src/components/ui/` и написать 11 компонентов.
4. Проверить сборку (`npm run build`).

**Статус:** ✅ Выполнено. 12 новых файлов.

### ✅ Шаг 2. Модальные окна
Заменить inline-обёртки на `<Modal>`:
- `CreateTaskModal` ✅
- `CreateSeatModal` ✅
- `ImportModal` ✅
- `TaskDetails` ✅
- `UserManagement` (2 модалки) ✅
- `GroupsManagement` (2 модалки) ✅
- `KnowledgeGraph` (document modal) ✅
- `KnowledgeBase` (fullscreen wrapper) ✅
- `ConfirmDialog` ✅
- `Agents` (2 модалки: Create + Run Detail) ✅ — дополнительно к плану

**Статус:** ✅ Выполнено. 10 файлов.

### ✅ Шаг 3. Alerts
Заменить inline error/success/info блоки на `<Alert type="...">` во всех файлах из раздела 1.2.

**Статус:** ✅ Выполнено. ~20 файлов. Включены файлы не из исходного плана: `Agents`, `Login`, `ResourceMonitor`, `AnalyticsCharts`.

### ✅ Шаг 4. Списки и фильтры
- `TaskFilters` → `<FilterPanel>` ✅
- `SeatFilters` → `<FilterPanel>` ✅
- `DocumentList` — спиннеры → `<Spinner>` ✅
- `SeatsList` — спиннеры → `<Spinner>` ✅
- `TasksList` — замена не потребовалась (нет inline-паттернов)

**Статус:** ✅ Выполнено. 4 файла. Полная замена на `<DataList>` не сделана — структура списков слишком различается (DocumentList с карточками, SeatsList с разделением my/other, TasksList с таблицей).

### ✅ Шаг 5. Карточки, кнопки, формы
- 20 файлов: inline `bg-white dark:bg-gray-800 rounded-lg shadow` → CSS-класс `.card` / `.card--padded` ✅
- Кнопки — **не сделано**: паттерны слишком вариативны (разный порядок классов, доп. классы `disabled:`, `transition`, `font-medium`), автоматическая замена небезопасна. CSS-классы `.btn--*` готовы, можно заменять вручную по мере необходимости.
- Бейджи — **не сделано**: аналогично, CSS-классы `.badge--*` готовы.

**Статус:** ✅ Частично выполнено. Карточки — да. Кнопки и бейджи — CSS-классы готовы, замена inline отложена.

### ✅ Шаг 6. Stats и прочее
- `ActivityTimeline.svelte` — удалён inline `<style>`, используется `.animate-fade-in` из CSS ✅
- Stats-гриды в `Admin`, `KnowledgeBase`, `Dashboard`, `Activity`, `SeatDetails` — **не сделано**: гриды содержат interactive onclick, не подходят для простой замены на `<StatsGrid>`. Компоненты `StatsGrid` и `StatCard` готовы для использования в новых местах.

**Статус:** ✅ Частично выполнено. Inline-стили удалены. Stats-гриды отложены.

### ✅ Шаг 7. Проверка
- `npm run build` — ✅ ошибок сборки нет
- `npm run check` (svelte-check) — 1 предсуществующая ошибка (`AdminTab` type), 0 новых
- Docker (`localhost:2002`) — ✅ запущен, healthy, API proxy работает
- CSS включает все семантические классы (59.98 КБ, +17 КБ от `components.css`)

**Статус:** ✅ Выполнено.

---

## 4. Оценка объёма (факт)

| Шаг | План | Факт | Статус |
|-----|------|------|--------|
| 1. CSS + UI-база | 11 новых | 12 новых | ✅ |
| 2. Модалки | 8 файлов | 10 файлов | ✅ |
| 3. Alerts | 15 файлов | ~20 файлов | ✅ |
| 4. Списки/фильтры | 5 файлов | 4 файла | ✅ |
| 5. Карточки/кнопки | 10 файлов | 20 файлов (только карточки) | ⚠️ частично |
| 6. Stats и прочее | 6 файлов | 1 файл | ⚠️ частично |
| 7. Проверка | — | — | ✅ |
| **Итого** | **~43 файла** | **~42 файла изменены, 12 создано** | |

---

## 5. Риски

- **Tailwind + custom CSS**: ✅ Решено. `@import` должен стоять **до** `@tailwind` директив, иначе `@apply` не компилируется. `resize-vertical` не существует в Tailwind → `resize-y`.
- **Svelte 5 runes**: ✅ подтверждено. Компоненты на `$props` / `$state`.
- **Chart.js / D3 в AnalyticsCharts и KnowledgeGraph**: ✅ затронуты только обёртки.
- **Docker-сети**: при пересоздании контейнера `web-ui` он может попасть в другую Docker-сеть. Исправлено через `docker network connect`.

---

## 6. Что осталось на будущее

- **Кнопки**: заменить inline Tailwind-классы на `.btn .btn--primary` и т.д. (классы уже в CSS)
- **Бейджи**: заменить inline-классы на `.badge .badge--green` и т.д. (классы уже в CSS)
- **StatsGrid**: заменить stats-гриды в `Admin`, `KnowledgeBase`, `Dashboard`, `Activity`, `SeatDetails` на `<StatsGrid>` + `<StatCard>` (потребует рефакторинга interactive onclick)
- **DataList**: полная замена `DocumentList`, `SeatsList`, `TasksList` на `<DataList>` (потребует приведения структуры к общему интерфейсу)
- **Form inputs**: заменить inline-классы инпутов на `.form-input`, `.form-select`, `.form-textarea`

---

## 7. Что НЕ входит в план

- Изменение логики работы с API
- Изменение маршрутизации
- Изменение структуры `src/pages` (кроме замены компонентов)
- Добавление новых функций
- Рефакторинг `src/lib/*`
