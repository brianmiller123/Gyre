import { createContext, useCallback, useContext, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import type { Severity } from '@/lib/agent/types'

/** A short-lived toast shown bottom-right (auto-dismisses). */
export interface Toast {
  id: string
  title: string
  body?: string
  severity: Severity
}

interface NotificationContextValue {
  toasts: Toast[]
  toast: (t: Omit<Toast, 'id'>, ttl?: number) => void
  dismissToast: (id: string) => void
}

const NotificationContext = createContext<NotificationContextValue | null>(null)

let counter = 0
const nextId = (prefix: string) => `${prefix}-${Date.now()}-${counter++}`

export function NotificationProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([])
  const timers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map())

  const dismissToast = useCallback((id: string) => {
    setToasts((prev) => prev.filter((t) => t.id !== id))
    const handle = timers.current.get(id)
    if (handle) {
      clearTimeout(handle)
      timers.current.delete(id)
    }
  }, [])

  const toast = useCallback(
    (t: Omit<Toast, 'id'>, ttl = 4200) => {
      const id = nextId('toast')
      setToasts((prev) => [...prev.slice(-3), { ...t, id }])
      const handle = setTimeout(() => dismissToast(id), ttl)
      timers.current.set(id, handle)
    },
    [dismissToast],
  )

  const value = useMemo(
    () => ({ toasts, toast, dismissToast }),
    [toasts, toast, dismissToast],
  )

  return <NotificationContext.Provider value={value}>{children}</NotificationContext.Provider>
}

export function useNotifications(): NotificationContextValue {
  const ctx = useContext(NotificationContext)
  if (!ctx) throw new Error('useNotifications must be used within NotificationProvider')
  return ctx
}
