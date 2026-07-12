import type { Config } from 'tailwindcss'

/**
 * Tailwind configuration for the C5 console.
 *
 * Theming strategy
 * ----------------
 * Semantic colors are mapped to CSS custom properties (rgb triplets) so the
 * entire palette can be re-skinned between light and dark by toggling the
 * `.dark` class on <html>. Using `<alpha-value>` lets every color support
 * Tailwind opacity modifiers (e.g. `bg-primary/20`).
 *
 * NOTE: declared as a typed const (not `satisfies`) so the Tailwind config
 * loader (jiti/sucrase) can parse it without transpilation surprises.
 */
const config: Config = {
  darkMode: 'class',
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        bg: 'rgb(var(--c-bg) / <alpha-value>)',
        surface: 'rgb(var(--c-surface) / <alpha-value>)',
        'surface-2': 'rgb(var(--c-surface-2) / <alpha-value>)',
        'surface-3': 'rgb(var(--c-surface-3) / <alpha-value>)',
        border: 'rgb(var(--c-border) / <alpha-value>)',
        'border-strong': 'rgb(var(--c-border-strong) / <alpha-value>)',
        text: 'rgb(var(--c-text) / <alpha-value>)',
        'text-2': 'rgb(var(--c-text-2) / <alpha-value>)',
        muted: 'rgb(var(--c-muted) / <alpha-value>)',
        primary: {
          DEFAULT: 'rgb(var(--c-primary) / <alpha-value>)',
          glow: 'rgb(var(--c-primary-glow) / <alpha-value>)',
        },
        accent: 'rgb(var(--c-accent) / <alpha-value>)',
        success: 'rgb(var(--c-success) / <alpha-value>)',
        warning: 'rgb(var(--c-warning) / <alpha-value>)',
        danger: 'rgb(var(--c-danger) / <alpha-value>)',
        info: 'rgb(var(--c-info) / <alpha-value>)',
      },
      fontFamily: {
        sans: ['"Plus Jakarta Sans"', 'ui-sans-serif', 'system-ui', 'sans-serif'],
        display: ['Sora', 'ui-sans-serif', 'system-ui', 'sans-serif'],
        mono: ['"JetBrains Mono"', 'ui-monospace', 'monospace'],
      },
      boxShadow: {
        soft: '0 1px 2px rgb(0 0 0 / .04), 0 6px 20px rgb(0 0 0 / .06)',
        pop: '0 12px 40px rgb(0 0 0 / .14)',
        glow: '0 0 0 1px rgb(var(--c-primary) / .22), 0 10px 36px rgb(var(--c-primary-glow) / .22)',
      },
      borderRadius: {
        xl: '14px',
        '2xl': '20px',
      },
      keyframes: {
        'fade-in': { '0%': { opacity: '0' }, '100%': { opacity: '1' } },
        'slide-up': {
          '0%': { opacity: '0', transform: 'translateY(10px)' },
          '100%': { opacity: '1', transform: 'translateY(0)' },
        },
        'scale-in': {
          '0%': { opacity: '0', transform: 'scale(.96)' },
          '100%': { opacity: '1', transform: 'scale(1)' },
        },
        shimmer: { '100%': { transform: 'translateX(100%)' } },
        'pulse-ring': {
          '0%': { transform: 'scale(.8)', opacity: '.7' },
          '100%': { transform: 'scale(2.2)', opacity: '0' },
        },
        'slide-left': { '0%': { transform: 'translateX(-100%)' }, '100%': { transform: 'translateX(0)' } },
        'slide-right': { '0%': { transform: 'translateX(100%)' }, '100%': { transform: 'translateX(0)' } },
      },
      animation: {
        'fade-in': 'fade-in .4s ease both',
        'slide-up': 'slide-up .55s cubic-bezier(.16,1,.3,1) both',
        'scale-in': 'scale-in .22s ease both',
        'pulse-ring': 'pulse-ring 1.8s cubic-bezier(.16,1,.3,1) infinite',
        'slide-left': 'slide-left .28s cubic-bezier(.16,1,.3,1) both',
        'slide-right': 'slide-right .28s cubic-bezier(.16,1,.3,1) both',
      },
    },
  },
  plugins: [],
}

export default config
