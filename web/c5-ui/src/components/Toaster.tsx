import { createPortal } from 'react-dom'
import type { Severity } from '@/types'
import { Icon } from '@/components/icons'
import { useNotifications, type Toast } from '@/lib/notifications'
import { cn } from '@/lib/cn'

const toastMeta: Record<Severity, { icon: string; ring: string; text: string }> = {
  success: { icon: 'check-circle', ring: 'bg-success', text: 'text-success' },
  info: { icon: 'info', ring: 'bg-info', text: 'text-info' },
  warning: { icon: 'alert', ring: 'bg-warning', text: 'text-warning' },
  danger: { icon: 'x-circle', ring: 'bg-danger', text: 'text-danger' },
}

/** Bottom-right stack of transient toasts, driven by useNotifications(). */
export function Toaster() {
  const { toasts, dismissToast } = useNotifications()
  if (toasts.length === 0) return null
  return createPortal(
    <div className="pointer-events-none fixed bottom-4 right-4 z-[120] flex w-full max-w-sm flex-col gap-2">
      {toasts.map((t) => (
        <ToastCard key={t.id} toast={t} onClose={() => dismissToast(t.id)} />
      ))}
    </div>,
    document.body,
  )
}

function ToastCard({ toast, onClose }: { toast: Toast; onClose: () => void }) {
  const meta = toastMeta[toast.severity]
  return (
    <div
      role="status"
      className="pointer-events-auto relative flex items-start gap-3 overflow-hidden rounded-xl border border-border bg-surface/95 p-3.5 pr-9 shadow-pop backdrop-blur-xl animate-slide-up"
    >
      <span className={cn('absolute left-0 top-0 h-full w-1', meta.ring)} />
      <span className={cn('mt-0.5', meta.text)}>
        <Icon name={meta.icon} size={18} />
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium text-text">{toast.title}</p>
        {toast.body && <p className="mt-0.5 text-[13px] text-muted">{toast.body}</p>}
      </div>
      <button
        onClick={onClose}
        className="absolute right-2 top-2.5 flex h-6 w-6 items-center justify-center rounded-md text-muted transition-colors hover:bg-surface-2 hover:text-text"
        aria-label="关闭"
      >
        <Icon name="close" size={15} />
      </button>
    </div>
  )
}
