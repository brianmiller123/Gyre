import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'

export type Mode = 'code' | 'architect' | 'ask' | 'debug'

export interface Settings {
  /** Origin of the agent server (e.g. http://localhost:8080). Defaults to the page origin. */
  serverUrl: string
  /** Optional auth token (appended as ?token= when the server requires it). */
  token: string
  /** Advisory mode sent with new tasks. */
  mode: Mode
  /** Selected model alias (applied when creating a session). null = server default. */
  model: string | null
  /** SOCKS5 代理开关的本地镜像（权威状态在服务端 /api/socks5；此值用于刷新前乐观显示）。 */
  socks5Enabled: boolean
}

const STORAGE_KEY = 'agent-ui-settings'

function defaults(): Settings {
  const origin = typeof window !== 'undefined' ? window.location.origin : ''
  return { serverUrl: origin, token: '', mode: 'code', model: null, socks5Enabled: false }
}

function load(): Settings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (raw) return { ...defaults(), ...JSON.parse(raw) }
  } catch {
    /* ignore */
  }
  return defaults()
}

interface SettingsContextValue {
  settings: Settings
  update: (patch: Partial<Settings>) => void
  reset: () => void
}

const SettingsContext = createContext<SettingsContextValue | null>(null)

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [settings, setSettings] = useState<Settings>(load)

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(settings))
    } catch {
      /* ignore */
    }
  }, [settings])

  const update = useCallback(
    (patch: Partial<Settings>) => setSettings((s) => ({ ...s, ...patch })),
    [],
  )
  const reset = useCallback(() => setSettings(defaults()), [])

  const value = useMemo(() => ({ settings, update, reset }), [settings, update, reset])
  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>
}

export function useSettings(): SettingsContextValue {
  const ctx = useContext(SettingsContext)
  if (!ctx) throw new Error('useSettings must be used within SettingsProvider')
  return ctx
}
