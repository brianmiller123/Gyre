import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from 'react'
import type { ReactNode } from 'react'
import {
  DEFAULT_LOCALE,
  SUPPORTED_LOCALES,
  locales,
} from '@/lib/locales'
import type { LocaleCode } from '@/lib/locales'

const STORAGE_KEY = 'agent-ui-locale'

/**
 * 把浏览器/系统语言标签归一为已支持语言码。
 *
 * - 接受 `zh-CN`/`zh_TW`/`en-US`/`ru`/`ja-JP` 等任意 BCP-47 形式。
 * - 三字母码（`zho`/`eng`/`rus`/`jpn`）一并兼容。
 * - 未匹配回退 [`DEFAULT_LOCALE`]（英文）。
 */
export function matchLocale(tag: string | undefined | null): LocaleCode {
  if (!tag) return DEFAULT_LOCALE
  const primary = tag.split(/[-_.@]/)[0]?.toLowerCase().trim()
  if (!primary) return DEFAULT_LOCALE
  switch (primary) {
    case 'zh':
    case 'zho':
    case 'chi':
      return 'zh'
    case 'ru':
    case 'rus':
      return 'ru'
    case 'ja':
    case 'jpn':
      return 'ja'
    case 'en':
    case 'eng':
      return 'en'
    default:
      return DEFAULT_LOCALE
  }
}

/** 探测系统语言（`navigator.languages`/`navigator.language`）。 */
export function detectSystemLocale(): LocaleCode {
  if (typeof navigator === 'undefined') return DEFAULT_LOCALE
  const candidates = navigator.languages?.length
    ? navigator.languages
    : [navigator.language]
  for (const c of candidates) {
    const matched = matchLocale(c)
    if (matched !== DEFAULT_LOCALE || (c && c.toLowerCase().startsWith('en'))) {
      return matched
    }
  }
  return DEFAULT_LOCALE
}

/** 读取持久化的语言选择（`'auto'` 表示跟随系统）。 */
function loadStored(): LocaleCode | 'auto' {
  try {
    const v = localStorage.getItem(STORAGE_KEY)
    if (v === 'auto' || SUPPORTED_LOCALES.includes(v as LocaleCode)) {
      return v as LocaleCode | 'auto'
    }
  } catch {
    /* storage 可能不可用（隐私模式）—— 忽略 */
  }
  return 'auto'
}

/** 解析最终生效语言：显式选择优先，否则跟随系统。 */
function resolveLocale(pref: LocaleCode | 'auto'): LocaleCode {
  return pref === 'auto' ? detectSystemLocale() : pref
}

export interface I18nContextValue {
  /** 当前生效语言码（已解析，非 `'auto'`）。 */
  locale: LocaleCode
  /** 用户偏好（`'auto'` 表示跟随系统）。 */
  preference: LocaleCode | 'auto'
  /** 设置语言偏好（`'auto'` 跟随系统）；持久化到 localStorage。 */
  setPreference: (pref: LocaleCode | 'auto') => void
  /** 取词并完成 `{name}` 命名插值；缺失回退英文，再缺失返回 key 本身。 */
  t: (key: string, args?: Record<string, string | number>) => string
}

const I18nContext = createContext<I18nContextValue | null>(null)

/**
 * I18nProvider 持有语言偏好，默认跟随系统（`navigator.languages`），
 * 用户显式选择后持久化到 localStorage。同时监听系统语言变化（偏好为 auto 时即时反映）。
 *
 * Provider 栈建议置于最外层（theme 之前/之后均可），让所有面板都能取词。
 */
export function I18nProvider({ children }: { children: ReactNode }) {
  const [preference, setPreferenceState] = useState<LocaleCode | 'auto'>(loadStored)
  const [locale, setLocale] = useState<LocaleCode>(() => resolveLocale(loadStored()))

  // 持久化偏好
  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, preference)
    } catch {
      /* ignore */
    }
    setLocale(resolveLocale(preference))
  }, [preference])

  // 偏好为 auto 时，监听系统语言变化（如用户切换 OS 语言）即时反映。
  useEffect(() => {
    if (preference !== 'auto' || typeof window === 'undefined') return
    const onChange = () => setLocale(detectSystemLocale())
    window.addEventListener('languagechange', onChange)
    return () => window.removeEventListener('languagechange', onChange)
  }, [preference])

  // 同步 <html lang>，便于无障碍与搜索引擎。
  useEffect(() => {
    document.documentElement.lang = locale
  }, [locale])

  const setPreference = useCallback((pref: LocaleCode | 'auto') => {
    setPreferenceState(pref)
  }, [])

  const t = useCallback(
    (key: string, args?: Record<string, string | number>) => {
      const dict = locales[locale] ?? locales[DEFAULT_LOCALE]
      let raw = dict[key] ?? locales[DEFAULT_LOCALE][key] ?? key
      if (args && raw.includes('{')) {
        for (const [name, val] of Object.entries(args)) {
          const ph = `{${name}}`
          if (raw.includes(ph)) raw = raw.split(ph).join(String(val))
        }
      }
      return raw
    },
    [locale],
  )

  const value = useMemo<I18nContextValue>(
    () => ({ locale, preference, setPreference, t }),
    [locale, preference, setPreference, t],
  )

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nContextValue {
  const ctx = useContext(I18nContext)
  if (!ctx) throw new Error('useI18n must be used within I18nProvider')
  return ctx
}
