import { useEffect, useState } from 'react'
import { useDeferredValue } from 'react'
// highlight.js core + languages are bundled locally (no CDN at runtime).
import hljs from 'highlight.js/lib/core'
// 两套主题以字符串内联打进主包（各 ~2KB），运行时按应用明暗模式挂载其一：
// 浅色代码块配 github 亮色令牌，深色配 github-dark，避免全局静态单主题
// 在另一模式下底色/字色不可读。
import githubDarkCss from 'highlight.js/styles/github-dark.min.css?inline'
import githubLightCss from 'highlight.js/styles/github.min.css?inline'
import { useTheme } from '@/lib/theme'

/**
 * 语言模块按需加载：只静态打包 core（很小），29 种语言各自成为独立 chunk，
 * 首次需要高亮时并行 `import()` 并注册。此前全部静态 import 进主 bundle，
 * 489KB 的产物里语言定义占了大头，而首屏对话流往往一个代码块都没有。
 */
const LANGUAGE_LOADERS: Record<string, () => Promise<{ default: any }>> = {
  rust: () => import('highlight.js/lib/languages/rust'),
  typescript: () => import('highlight.js/lib/languages/typescript'),
  javascript: () => import('highlight.js/lib/languages/javascript'),
  json: () => import('highlight.js/lib/languages/json'),
  yaml: () => import('highlight.js/lib/languages/yaml'),
  ini: () => import('highlight.js/lib/languages/ini'),
  markdown: () => import('highlight.js/lib/languages/markdown'),
  python: () => import('highlight.js/lib/languages/python'),
  go: () => import('highlight.js/lib/languages/go'),
  java: () => import('highlight.js/lib/languages/java'),
  c: () => import('highlight.js/lib/languages/c'),
  cpp: () => import('highlight.js/lib/languages/cpp'),
  csharp: () => import('highlight.js/lib/languages/csharp'),
  bash: () => import('highlight.js/lib/languages/bash'),
  xml: () => import('highlight.js/lib/languages/xml'),
  css: () => import('highlight.js/lib/languages/css'),
  scss: () => import('highlight.js/lib/languages/scss'),
  sql: () => import('highlight.js/lib/languages/sql'),
  php: () => import('highlight.js/lib/languages/php'),
  ruby: () => import('highlight.js/lib/languages/ruby'),
  kotlin: () => import('highlight.js/lib/languages/kotlin'),
  swift: () => import('highlight.js/lib/languages/swift'),
  scala: () => import('highlight.js/lib/languages/scala'),
  lua: () => import('highlight.js/lib/languages/lua'),
  graphql: () => import('highlight.js/lib/languages/graphql'),
  dockerfile: () => import('highlight.js/lib/languages/dockerfile'),
  makefile: () => import('highlight.js/lib/languages/makefile'),
  protobuf: () => import('highlight.js/lib/languages/protobuf'),
}

let registered = false
let registering: Promise<void> | null = null

let hljsStyleEl: HTMLStyleElement | null = null

/** 把 highlight.js 主题（github / github-dark）挂到文档级 <style> 上（幂等）。 */
export function applyHljsTheme(theme: 'light' | 'dark'): void {
  if (typeof document === 'undefined') return
  hljsStyleEl ??= Object.assign(document.createElement('style'), { id: 'hljs-theme' })
  if (!hljsStyleEl.isConnected) document.head.appendChild(hljsStyleEl)
  hljsStyleEl.textContent = theme === 'dark' ? githubDarkCss : githubLightCss
}

/** 组件侧入口：跟随 ThemeProvider 切换 highlight.js 主题。 */
export function useHljsTheme(): void {
  const { theme } = useTheme()
  useEffect(() => {
    applyHljsTheme(theme)
  }, [theme])
}

/** 注册全部支持的语言（幂等；首次调用并行拉取语言 chunk）。 */
export function ensureLanguages(): Promise<void> {
  if (registered) return Promise.resolve()
  registering ??= Promise.all(
    Object.entries(LANGUAGE_LOADERS).map(async ([name, load]) => {
      hljs.registerLanguage(name, (await load()).default)
    }),
  ).then(() => {
    registered = true
  })
  return registering
}

/**
 * The hook keeps a `ready` flag (true once language chunks are registered) so
 * callers can defer the first paint imperceptibly; no network is involved.
 *
 * Also keeps the highlight.js theme in sync with the app theme: the light
 * app mode renders code on the light `--c-code-bg`, so the github light theme
 * must be mounted there (see `applyHljsTheme`).
 */
export function useHighlighter(theme: 'light' | 'dark'): boolean {
  useEffect(() => {
    applyHljsTheme(theme)
  }, [theme])
  const [ready, setReady] = useState(registered)
  useEffect(() => {
    if (registered) return
    let on = true
    void ensureLanguages().then(() => on && setReady(true))
    return () => {
      on = false
    }
  }, [])
  return ready
}

/** Highlight a code string for the given language; returns safe HTML. */
export async function highlight(code: string, lang: string): Promise<string> {
  await ensureLanguages()
  try {
    if (lang && lang !== 'plaintext' && hljs.getLanguage(lang)) {
      return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value
    }
    return hljs.highlightAuto(code).value
  } catch {
    return escapeHtml(code)
  }
}

/**
 * React 侧统一入口：deferred code（流式期间合并高亮帧）→ 异步高亮 → 带
 * 过期结果守卫地 setState。初始值是转义后的纯文本，语言 chunk 到位前代码块
 * 仍立即可读（无空白闪烁）；流式期间展示 deferred 内容的高亮（与原实现一致）。
 */
export function useHighlightedCode(code: string, lang: string): string {
  const deferred = useDeferredValue(code)
  const [html, setHtml] = useState(() => escapeHtml(code))
  useEffect(() => {
    let on = true
    void highlight(deferred, lang).then((h) => {
      if (on) setHtml(h)
    })
    return () => {
      on = false
    }
  }, [deferred, lang])
  return html
}

function escapeHtml(s: string): string {
  // 字符 → HTML 实体。此前实现误把实体写成原字符（空操作）：未转义内容经 innerHTML
  // 注入会执行（XSS），此路径在 hljs 抛异常时兜底使用。
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}
