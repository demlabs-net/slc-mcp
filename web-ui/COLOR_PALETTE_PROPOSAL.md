# Предложение: цветовые палитры для SLC Dashboard

> Не внедрено. Ожидает одобрения.  
> Текущая палитра описана в `src/app.css` (Phase 1 redesign).

---

## Текущая палитра (проблемы)

```
Dark:  bg-primary: #09090b   bg-secondary: #111113   bg-tertiary: #18181b
Light: bg-primary: #fafafa   bg-secondary: #ffffff   bg-tertiary: #f4f4f5
Accent: #8b5cf6 (purple)     Primary: #3b82f6 (blue)
```

**Проблемы:**
- Тёмная тема: слишком «холодный» серый — отдаёт Bootstrap/Tailwind defaults
- Нет теплоты — выглядит generic, не premium
- Светлая тема: белый слишкой «плоский», нет depth
- Один accent — нет accent-ai subtle, accent-ai foreground
- Нет surface-overlay, surface-dialog для модалок/dropdown

---

## Вариант A: «Linear-style» (Premium Dark)

Вдохновение: **Linear, Raycast** — глубокие тёплые чёрные, subtle warm undertone.

### Dark Theme
```css
:root {
  /* Surfaces — warm blacks with subtle blue-gray tint */
  --bg-primary:      #0a0a0c;   /* основной фон — почти чёрный с warm tint */
  --bg-secondary:    #111114;   /* карточки, sidebar */
  --bg-tertiary:     #19191d;   /* inputs, hover states */
  --bg-elevated:     #222228;   /* dropdowns, tooltips, popovers */
  --bg-overlay:      rgba(0,0,0,0.7); /* modal backdrop */

  /* Borders — very subtle, barely visible */
  --border-subtle:   #ffffff0a; /* white с 4% opacity — едва заметная */
  --border-default:  #ffffff14; /* white с 8% opacity */
  --border-focus:    #8b5cf6;   /* AI accent для фокуса */

  /* Text — high contrast */
  --text-primary:    #ededef;   /* не чистый белый — мягче */
  --text-secondary:  #8b8b93;   /* muted text */
  --text-tertiary:   #5a5a63;   /* very muted */
  --text-inverse:    #0a0a0c;   /* для текста на accent фоне */

  /* Accents */
  --accent-ai:            #8b5cf6;  /* фиолетовый — основной AI */
  --accent-ai-hover:      #a78bfa;
  --accent-ai-subtle:     #8b5cf615; /* 8% opacity */
  --accent-ai-foreground: #f0ecff;   /* текст на AI фоне */
  --accent-primary:       #3e63dd;   /* синий — чуть приглушённее */
  --accent-primary-hover: #5a7cf5;
  --accent-success:       #30a46c;   /* зелёный — muted */
  --accent-error:         #e5484d;   /* красный — softer */
  --accent-warning:       #f5a623;   /* янтарный — теплее */

  /* Shadows — с warm tint */
  --shadow-sm: 0 1px 2px rgba(0,0,0,0.4);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.5);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.6);
  --shadow-glow: 0 0 20px rgba(139,92,246,0.2);
}
```

### Light Theme
```css
.light {
  /* Surfaces — clean whites with subtle warmth */
  --bg-primary:      #fdfcfd;   /* не чистый белый — faint pink tint */
  --bg-secondary:    #ffffff;
  --bg-tertiary:     #f5f4f6;   /* subtle lavender-gray */
  --bg-elevated:     #ffffff;
  --bg-overlay:      rgba(0,0,0,0.4);

  /* Borders */
  --border-subtle:   #e8e7ec;
  --border-default:  #d6d5db;
  --border-focus:    #8b5cf6;

  /* Text */
  --text-primary:    #1a1a22;   /* не чистый чёрный — мягче */
  --text-secondary:  #63636e;
  --text-tertiary:   #8f8f9d;
  --text-inverse:    #ffffff;

  /* Accents — same hue, adjusted saturation */
  --accent-ai:            #7c5ae8;
  --accent-ai-hover:      #6b4ad4;
  --accent-ai-subtle:     #7c5ae812;
  --accent-ai-foreground: #ffffff;
  --accent-primary:       #3358cc;
  --accent-primary-hover: #2a4ab5;
  --accent-success:       #2d9d61;
  --accent-error:         #d93f3f;
  --accent-warning:       #e5941a;

  /* Shadows */
  --shadow-sm: 0 1px 2px rgba(0,0,0,0.06);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.08);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.12);
  --shadow-glow: 0 0 20px rgba(124,90,232,0.15);
}
```

**Характер:** Приглушённый, профессиональный, «enterprise AI tool».  
**Референсы:** Linear, Raycast, Notion dark.

---

## Вариант B: «Vercel-style» (Clean & Technical)

Вдохновение: **Vercel Dashboard, Next.js docs** — нейтральные серые, zero warm tint, technical feel.

### Dark Theme
```css
:root {
  /* Surfaces — neutral grays, no warm/cool bias */
  --bg-primary:      #000000;   /* pure black — signature Vercel */
  --bg-secondary:    #0a0a0a;
  --bg-tertiary:     #141414;
  --bg-elevated:     #1a1a1a;
  --bg-overlay:      rgba(0,0,0,0.8);

  /* Borders */
  --border-subtle:   #1f1f1f;
  --border-default:  #333333;
  --border-focus:    #0070f3;

  /* Text */
  --text-primary:    #fafafa;
  --text-secondary:  #888888;
  --text-tertiary:   #666666;
  --text-inverse:    #000000;

  /* Accents */
  --accent-ai:            #8b5cf6;
  --accent-ai-hover:      #a78bfa;
  --accent-ai-subtle:     #8b5cf612;
  --accent-ai-foreground: #fafafe;
  --accent-primary:       #0070f3;   /* signature Vercel blue */
  --accent-primary-hover: #1a85ff;
  --accent-success:       #0cce6b;
  --accent-error:         #ee0000;
  --accent-warning:       #f5a623;

  --shadow-sm: 0 1px 2px rgba(0,0,0,0.5);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.6);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.7);
  --shadow-glow: 0 0 30px rgba(0,112,243,0.15);
}
```

### Light Theme
```css
.light {
  --bg-primary:      #ffffff;
  --bg-secondary:    #ffffff;
  --bg-tertiary:     #fafafa;
  --bg-elevated:     #ffffff;
  --bg-overlay:      rgba(0,0,0,0.5);

  --border-subtle:   #eaeaea;
  --border-default:  #d9d9d9;
  --border-focus:    #0070f3;

  --text-primary:    #000000;
  --text-secondary:  #666666;
  --text-tertiary:   #999999;
  --text-inverse:    #ffffff;

  --accent-ai:            #7c3aed;
  --accent-ai-hover:      #6d28d9;
  --accent-ai-subtle:     #7c3aed10;
  --accent-ai-foreground: #ffffff;
  --accent-primary:       #0070f3;
  --accent-primary-hover: #005bd4;
  --accent-success:       #00a63e;
  --accent-error:         #cc0000;
  --accent-warning:       #e5941a;

  --shadow-sm: 0 1px 2px rgba(0,0,0,0.05);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.07);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.1);
  --shadow-glow: 0 0 30px rgba(0,112,243,0.1);
}
```

**Характер:** Максимально чистый, technical, «zero personality» — фокус на контенте.  
**Референсы:** Vercel, Next.js, Turbo.

---

## Вариант C: «Neon AI» (Bold & Vibrant)

Вдохновение: **Bolt.new, Lovable, v0** — яркие акценты, glassmorphism, «AI-first energy».

### Dark Theme
```css
:root {
  /* Surfaces — deep purple-tinted blacks */
  --bg-primary:      #07060b;   /* faint purple undertone */
  --bg-secondary:    #0e0d14;
  --bg-tertiary:     #161520;
  --bg-elevated:     #1e1d2a;
  --bg-overlay:      rgba(7,6,11,0.85);

  /* Borders — purple-tinted */
  --border-subtle:   rgba(139,92,246,0.08);
  --border-default:  rgba(139,92,246,0.15);
  --border-focus:    #a78bfa;

  /* Text */
  --text-primary:    #f0ecff;   /* faint purple tint */
  --text-secondary:  #9d97b8;
  --text-tertiary:   #6b6484;
  --text-inverse:    #07060b;

  /* Accents — vibrant */
  --accent-ai:            #a78bfa;   /* brighter purple */
  --accent-ai-hover:      #c4b5fd;
  --accent-ai-subtle:     #a78bfa18;
  --accent-ai-foreground: #07060b;
  --accent-primary:       #6366f1;   /* indigo */
  --accent-primary-hover: #818cf8;
  --accent-success:       #34d399;   /* bright emerald */
  --accent-error:         #fb7185;   /* rose */
  --accent-warning:       #fbbf24;   /* amber */

  --shadow-sm: 0 1px 2px rgba(0,0,0,0.4);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.5);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.6);
  --shadow-glow: 0 0 30px rgba(167,139,250,0.25);
}
```

### Light Theme
```css
.light {
  --bg-primary:      #faf8ff;   /* purple-tinted white */
  --bg-secondary:    #ffffff;
  --bg-tertiary:     #f3f0ff;
  --bg-elevated:     #ffffff;
  --bg-overlay:      rgba(7,6,11,0.5);

  --border-subtle:   #e8e3f5;
  --border-default:  #d4cced;
  --border-focus:    #a78bfa;

  --text-primary:    #160e2e;
  --text-secondary:  #5e5578;
  --text-tertiary:   #8e85a5;
  --text-inverse:    #ffffff;

  --accent-ai:            #8b5cf6;
  --accent-ai-hover:      #7c3aed;
  --accent-ai-subtle:     #8b5cf612;
  --accent-ai-foreground: #ffffff;
  --accent-primary:       #5b5bd6;
  --accent-primary-hover: #4f46e5;
  --accent-success:       #16a34a;
  --accent-error:         #e11d48;
  --accent-warning:       #d97706;

  --shadow-sm: 0 1px 2px rgba(0,0,0,0.05);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.08);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.1);
  --shadow-glow: 0 0 30px rgba(139,92,246,0.15);
}
```

**Характер:** Энергичный, «AI-native», выделяется на рынке.  
**Референсы:** Bolt.new, Lovable, Cursor marketing.

---

## Сравнение

| Критерий | A: Linear | B: Vercel | C: Neon AI |
|----------|-----------|-----------|------------|
| **Характер** | Приглушённый, enterprise | Чистый, technical | Яркий, AI-first |
| **Тёмная тема** | Тёплый чёрный | Нейтральный чёрный | Фиолетовый чёрный |
| **Светлая тема** | Lavender-gray | Pure white | Purple-tinted |
| **Акценты** | Muted | Technical | Vibrant |
| **Для кого** | Enterprise/B2B | Dev tools, infra | AI startups |
| **Риск** | Безопасный | Безопасный | Смелый |
| **Референсы** | Linear, Raycast | Vercel, Next.js | Bolt, Lovable |

---

## Рекомендация

Для SLC (AI-агенты, knowledge graph, задачи) рекомендую **Вариант A (Linear-style)**:
- Выглядит premium и профессионально
- Подходит для enterprise-продукта с AI
- Не polarizing — нравится большинству
- Лучшая читаемость в dark mode
- Приглушённые акценты не отвлекают от контента

Если продукт позиционируется как bold/innovative → **Вариант C (Neon AI)**.
Если аудитория — разработчики инфраструктуры → **Вариант B (Vercel-style)**.

---

## Как внедрить

Замена в одном файле: `src/app.css` (строки 9–41) — перезаписать CSS variables.
Все компоненты автоматически подхватят новую палитру через `var(--*)`.
Потребуется также обновить `tailwind.config.js` если меняются accent цвета.
