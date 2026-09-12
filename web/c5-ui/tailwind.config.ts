import type { Config } from 'tailwindcss'

/**
 * Tailwind configuration for the Agent Console.
 *
 * Theming strategy
 * ----------------
 * Semantic colors are mapped to CSS custom properties (rgb triplets) so the
 * entire palette can be re-skinned between light and dark by toggling the
 * `.dark` class on <html>. Using `<alpha-value>` lets every color support
 * Tailwind opacity modifiers (e.g. `bg-primary/20`).
 *
 * Design-token strategy (v2 "Console")
 * ------------------------------------
 * Three scales are *closed sets* — new arbitrary values are not welcome:
 *
 *   Type    2xs 11 · xs 12 · sm 13 · base 14 · md 15 · lg 17 · xl 20 · 2xl 24 · 3xl 30
 *   Radius  sm 4 · md 8 · lg 10 · xl 14 · 2xl 20 · full
 *   Icons   12 · 14 · 16 · 18 · 20 · 26   (see components/icons.tsx)
 *
 * Before v2 the app shipped 7 ad-hoc font sizes (`text-[9.5px]` … `text-[15px]`),
 * 6 radii and 9 different translucent surface alphas, which is exactly what made
 * the chrome read as "noisy": every panel had its own private rhythm. Anything
 * outside these scales should be expressed with these tokens instead.
 *
 * NOTE: declared as a typed const (not `satisfies`) so the Tailwind config
 * loader (jiti/sucrase) can parse it without transpilation surprises.
 */
const config: Config = {
  darkMode: 'class',
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      /* 超宽屏断点：1920px 以上再放宽一档对话列（2xl 只到 1536）。 */
      screens: {
        '3xl': '1920px',
      },
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
        'code-bg': 'rgb(var(--c-code-bg) / <alpha-value>)',
        'code-fg': 'rgb(var(--c-code-fg) / <alpha-value>)',
        primary: {
          DEFAULT: 'rgb(var(--c-primary) / <alpha-value>)',
          glow: 'rgb(var(--c-primary-glow) / <alpha-value>)',
          /* 压在 bg-primary 上的前景色：由 applyAccent 按对比度择优写入，
             保证任意 accent × 任意主题下按钮/气泡文字都可读。 */
          fg: 'rgb(var(--c-on-primary) / <alpha-value>)',
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
      /* 字体层级：闭合的 9 档，带行高 —— 禁用 text-[Npx] 任意值。 */
      fontSize: {
        '2xs': ['11px', { lineHeight: '15px' }],
        xs: ['12px', { lineHeight: '17px' }],
        sm: ['13px', { lineHeight: '19px' }],
        base: ['14px', { lineHeight: '21px' }],
        md: ['15px', { lineHeight: '23px' }],
        lg: ['17px', { lineHeight: '24px' }],
        xl: ['20px', { lineHeight: '28px' }],
        '2xl': ['24px', { lineHeight: '32px' }],
        '3xl': ['30px', { lineHeight: '36px' }],
      },
      /* 间距体系：面板内边距 / 区块间距各只有一个值，避免就近取数。 */
      spacing: {
        panel: '1.25rem', // 20px — 面板内边距
        section: '1.5rem', // 24px — 区块之间
      },
      /* 圆角：4 档 + full。md 小控件、lg 控件、xl 卡片、2xl 浮层。 */
      borderRadius: {
        sm: '4px',
        md: '8px',
        lg: '10px',
        xl: '14px',
        '2xl': '20px',
      },
      boxShadow: {
        /* 三级高度：静置卡片 → 悬浮面板 → 模态；glow 仅用于品牌/强调反馈。 */
        card: '0 1px 2px rgb(0 0 0 / .04), 0 1px 3px rgb(0 0 0 / .04)',
        soft: '0 1px 2px rgb(0 0 0 / .04), 0 6px 20px rgb(0 0 0 / .06)',
        pop: '0 12px 40px rgb(0 0 0 / .14)',
        glow: '0 0 0 1px rgb(var(--c-primary) / .22), 0 10px 36px rgb(var(--c-primary-glow) / .22)',
      },
      /* Elevation layers: one token per overlay kind — never bare z-[N] values.
         `raised` 是唯一的例外档：它表达的**不是**高度，而是「已进入某个浮层层叠
         上下文后，需要盖住同容器内兄弟元素」的局部绘制顺序（如弹窗面板盖住遮罩、
         图表 tooltip 盖住柱子）。把它令牌化后，全库不再出现裸 z-10。 */
      zIndex: {
        raised: '10',
        /* 顶栏自带 backdrop-blur（会建立层叠上下文），其溢出菜单必须整体抬到
           滚动内容之上，否则会被后面的对话流盖住。 */
        header: '20',
        dropdown: '50',
        palette: '70',
        stats: '80',
        drawer: '90',
        workspace: '95',
        modal: '100',
        toast: '120',
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
        'palette-in': {
          '0%': { opacity: '0', transform: 'translateY(-8px) scale(.985)' },
          '100%': { opacity: '1', transform: 'translateY(0) scale(1)' },
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
        'palette-in': 'palette-in .18s cubic-bezier(.16,1,.3,1) both',
        'pulse-ring': 'pulse-ring 1.8s cubic-bezier(.16,1,.3,1) infinite',
        'slide-left': 'slide-left .28s cubic-bezier(.16,1,.3,1) both',
        'slide-right': 'slide-right .28s cubic-bezier(.16,1,.3,1) both',
      },
    },
  },
  plugins: [],
}

export default config
