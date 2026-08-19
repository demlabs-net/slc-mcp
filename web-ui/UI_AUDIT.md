# Аудит UI: что выглядит устаревшим и как исправить

> Дата: 15.06.2026. Ветка: `ui-refactor`.

---

## 1. Диагноз: почему интерфейс выглядит 2022 года

### 1.1. «Плоские» карточки без глубины
**Где:** KnowledgeBase (9 мест), Seats (6), Agents (4), Admin (1)

Карточки используют `bg-white dark:bg-gray-800` + `border` — это дизайн-система 2020–2022 (Tailwind defaults). В 2026 стандарт — **многослойность**: subtle gradient backgrounds, glassmorphism, glow-shadows.

**Сейчас:**
```svelte
<div class="bg-white dark:bg-gray-800 rounded-lg border border-gray-200 dark:border-gray-700 p-6">
```

**Нужно:**
```svelte
<div class="rounded-xl p-6" style="background: linear-gradient(145deg, var(--bg-secondary), var(--bg-tertiary)); border: 1px solid var(--border-subtle); box-shadow: 0 1px 3px rgba(0,0,0,0.3);">
```

### 1.2. Жёсткие границы (hard borders)
**Где:** повсеместно — `border border-gray-200 dark:border-gray-700`

В 2026 году границы используются минимально. Вместо них: **разделение через фон** (контраст bg-secondary vs bg-primary) или **очень тонкие** `border-subtle` (1px, opacity 0.5).

### 1.3. Нет анимаций и micro-interactions
**Где:** все страницы

- Кнопки не имеют hover/press-эффектов (кроме цвета)
- Карточки не реагируют на hover
- Нет page transitions
- Нет skeleton loading states
- Нет typing/streaming animations

### 1.4. Typography: нет иерархии
**Где:** Dashboard, Admin, Agents

- Заголовки: `text-2xl font-bold` — стандартный, не выделяется
- Нет tracking (letter-spacing) для заголовков
- Нет различия в weight между заголовками и подзаголовками
- Subtitle-text того же размера что body

### 1.5. Пустые состояния (empty states) скучные
**Где:** Tasks (пустой список), Agents (нет выбора), KB (нет документов)

Простой текст «No items found» без визуала. В 2026: иллюстрация/анимация + CTA.

### 1.6. Кнопки — «default Tailwind style»
**Где:** повсеместно

`px-4 py-2 bg-blue-600 rounded-lg` — это стандартный Tailwind, мгновенно узнаваемый как «сделано на Tailwind». В 2026: custom pill-shape, gradient backgrounds, glow on hover.

### 1.7. Формы — стандартные inputs
**Где:** Login, CreateTaskModal, CreateSeatModal, Admin config

Обычные `border rounded-lg` inputs. В 2026: подсветка фокуса с glow, subtle background, floating labels.

### 1.8. Sidebar — базовый
**Где:** Layout.svelte

Sidebar работает, но:
- Нет tooltip при collapsed state
- Нет badge/counter для уведомлений
- Активный пункт — просто заливка, нет pill-shape или left-border accent
- Нет анимации collapse/expand

### 1.9. Нет depth/layers
Весь UI на одном уровне. В 2026: backdrop-blur для модалок, glassmorphism для dropdown, subtle parallax.

### 1.10. Статус-индикаторы примитивные
**Где:** Agents (enabled/disabled), SystemHealth (healthy/unhealthy)

Текстовые бейджи. В 2026: pulsing dots, animated progress rings, color-coded severity.

---

## 2. Конкретный план исправлений

### 2.1. Обновить карточки → glassmorphism + gradient

**Файлы:** все страницы с `bg-white dark:bg-gray-800`

Заменить на CSS-variable-based карточки с subtle gradient:

```css
/* components.css — обновить .card */
.card {
  background: linear-gradient(145deg, var(--bg-secondary), var(--bg-tertiary));
  border: 1px solid var(--border-subtle);
  border-radius: 16px; /* было 8px */
  box-shadow: 0 1px 3px rgba(0,0,0,0.3), 0 0 0 1px rgba(255,255,255,0.03);
  transition: all 0.2s ease;
}
.card:hover {
  box-shadow: 0 4px 12px rgba(0,0,0,0.4);
  border-color: var(--border-default);
}
```

**Оценка:** 2–3 ч (20 файлов)

### 2.2. Кнопки → custom pill-shape + gradient + glow

**Файлы:** components.css + все кнопки

```css
.btn--primary {
  background: linear-gradient(135deg, var(--accent-ai), #7c3aed);
  border-radius: 12px; /* pill-shape */
  font-weight: 500;
  letter-spacing: -0.01em;
  transition: all 0.15s ease;
  box-shadow: 0 1px 2px rgba(0,0,0,0.3);
}
.btn--primary:hover {
  box-shadow: 0 0 20px rgba(139,92,246,0.3);
  transform: translateY(-1px);
}
.btn--primary:active {
  transform: translateY(0);
  box-shadow: 0 0 10px rgba(139,92,246,0.2);
}
```

**Оценка:** 1–2 ч

### 2.3. Inputs → glow focus + subtle background

```css
.form-input {
  background: var(--bg-tertiary);
  border: 1px solid var(--border-subtle);
  border-radius: 10px;
  transition: all 0.15s ease;
}
.form-input:focus {
  border-color: var(--accent-ai);
  box-shadow: 0 0 0 3px rgba(139,92,246,0.15);
  outline: none;
}
```

**Оценка:** 1 ч

### 2.4. Typography → иерархия + tracking

```css
h1 { font-size: 1.75rem; font-weight: 700; letter-spacing: -0.025em; line-height: 1.2; }
h2 { font-size: 1.25rem; font-weight: 600; letter-spacing: -0.02em; line-height: 1.3; }
h3 { font-size: 1rem; font-weight: 600; letter-spacing: -0.01em; }
.subtitle { font-size: 0.875rem; color: var(--text-secondary); letter-spacing: 0; }
```

**Оценка:** 1 ч

### 2.5. Sidebar → polish

- Tooltip при collapsed: `<Tooltip>` при hover на иконку
- Active item: pill-shape background + left accent bar (3px purple)
- Badge counter на Agents (количество активных)
- Smooth collapse animation (transition: width 0.2s)

**Файл:** src/lib/Layout.svelte  
**Оценка:** 2–3 ч

### 2.6. Empty states → иллюстрации + CTA

```svelte
<!-- Вместо "No tasks found" -->
<div class="text-center py-16">
  <div class="w-20 h-20 mx-auto mb-6 rounded-2xl flex items-center justify-center"
       style="background: var(--accent-ai-subtle);">
    <Icon name="clipboard-list" size={40} />
  </div>
  <h3 class="text-lg font-semibold mb-2">No tasks yet</h3>
  <p class="text-sm mb-6" style="color: var(--text-secondary);">
    Create your first task to get started with AI agents
  </p>
  <button class="btn btn--primary">Create Task</button>
</div>
```

**Файлы:** Tasks, Agents, KB, Seats  
**Оценка:** 2 ч

### 2.7. Page transitions → fade + slide

```svelte
<!-- Router.svelte -->
{#key currentRoute}
  <div class="animate-fade-in">
    <PageComponent />
  </div>
{/key}
```

```css
.animate-fade-in {
  animation: fadeIn 0.15s ease-out;
}
@keyframes fadeIn {
  from { opacity: 0; transform: translateY(4px); }
  to { opacity: 1; transform: translateY(0); }
}
```

**Оценка:** 30 мин

### 2.8. Status indicators → pulsing dots + animated

```css
.status-dot {
  width: 8px; height: 8px;
  border-radius: 50%;
  position: relative;
}
.status-dot--active {
  background: var(--accent-success);
  animation: pulse 2s infinite;
}
@keyframes pulse {
  0%, 100% { box-shadow: 0 0 0 0 rgba(34,197,94,0.4); }
  50% { box-shadow: 0 0 0 6px rgba(34,197,94,0); }
}
```

**Файлы:** Agents, SystemHealth  
**Оценка:** 1 ч

### 2.9. Loading states → skeleton screens

```svelte
{#if loading}
  <div class="space-y-4">
    {#each Array(3) as _}
      <div class="h-16 rounded-xl animate-pulse" style="background: var(--bg-tertiary);"></div>
    {/each}
  </div>
{:else}
  <!-- content -->
{/if}
```

**Файлы:** Dashboard, Tasks, Agents, KB  
**Оценка:** 2 ч

### 2.10. Login → glassmorphism polish

Уже есть glassmorphism card. Доработать:
- Animated gradient background (slow-moving gradient)
- Input glow при фокусе
- Button hover glow
- Subtle scale animation при загрузке

**Оценка:** 1 ч

---

## 3. Сводная таблица

| Изменение | Файлов | Оценка | Приоритет |
|-----------|--------|--------|-----------|
| Карточки → gradient + glassmorphism | ~20 | 2–3 ч | 🔴 высокий |
| Кнопки → pill-shape + gradient + glow | ~15 | 1–2 ч | 🔴 высокий |
| Inputs → glow focus | ~10 | 1 ч | 🟡 средний |
| Typography → иерархия | ~10 | 1 ч | 🟡 средний |
| Sidebar → polish | 1 | 2–3 ч | 🟡 средний |
| Empty states → illustrations | ~5 | 2 ч | 🟡 средний |
| Page transitions | 1 | 30 мин | 🟢 низкий |
| Status indicators → pulsing | ~3 | 1 ч | 🟢 низкий |
| Loading → skeleton screens | ~5 | 2 ч | 🟢 низкий |
| Login → animated gradient | 1 | 1 ч | 🟢 низкий |
| **Итого** | **~40** | **13–16 ч** | |

---

## 4. Quick Wins (самый большой визуальный эффект за минимальное время)

1. **Карточки** — замена `bg-white dark:bg-gray-800` на gradient + border-radius 16px → мгновенно выглядит современно
2. **Кнопки** — gradient + glow hover → «premium feel»
3. **Inputs** — glow focus → ощущение интерактивности
4. **Page transitions** — fade-in при навигации → «живой» интерфейс
5. **Status dots** — pulsing animation → «AI работает»

Эти 5 вещей можно сделать за **4–5 часов** и получить **80% визуального улучшения**.
