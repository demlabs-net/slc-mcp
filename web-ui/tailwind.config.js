/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx,svelte}",
  ],
  theme: {
    extend: {
      colors: {
        ai: {
          DEFAULT: 'var(--accent-ai)',
          hover: 'var(--accent-ai-hover)',
          subtle: 'var(--accent-ai-subtle)',
          foreground: 'var(--accent-ai-foreground)',
        },
        primary: {
          DEFAULT: 'var(--accent-primary)',
          hover: 'var(--accent-primary-hover)',
        },
        success: 'var(--accent-success)',
        error: 'var(--accent-error)',
        warning: 'var(--accent-warning)',
        surface: {
          1: 'var(--bg-primary)',
          2: 'var(--bg-secondary)',
          3: 'var(--bg-tertiary)',
          elevated: 'var(--bg-elevated)',
          overlay: 'var(--bg-overlay)',
        },
        subtle: 'var(--border-subtle)',
        border: {
          DEFAULT: 'var(--border-default)',
          focus: 'var(--border-focus)',
        },
        content: {
          1: 'var(--text-primary)',
          2: 'var(--text-secondary)',
          3: 'var(--text-tertiary)',
          inverse: 'var(--text-inverse)',
        },
      },
      fontFamily: {
        sans: ['Geist Sans', 'system-ui', '-apple-system', 'sans-serif'],
        mono: ['Geist Mono', 'ui-monospace', 'Cascadia Code', 'monospace'],
      },
      boxShadow: {
        'sm': 'var(--shadow-sm)',
        'md': 'var(--shadow-md)',
        'lg': 'var(--shadow-lg)',
        'glow': 'var(--shadow-glow)',
      },
    },
  },
  plugins: [],
  darkMode: 'class',
}
