import { useState } from 'react'
import { Dropdown } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import type { ApprovalModeValue } from '@/lib/agent/useAgentSession'
import { useI18n } from '@/lib/i18n'
import { useNotifications } from '@/lib/notifications'
import { cn } from '@/lib/cn'

const MODES: { value: ApprovalModeValue; labelKey: string; shortKey: string }[] = [
  { value: 'always-ask', labelKey: 'settings.approval_always_ask', shortKey: 'settings.approval_short_always_ask' },
  { value: 'write', labelKey: 'settings.approval_write', shortKey: 'settings.approval_short_write' },
  { value: 'yolo', labelKey: 'settings.approval_yolo', shortKey: 'settings.approval_short_yolo' },
]

/**
 * Compact approval-mode switcher for the composer toolbar (above the input box).
 * Switching takes effect immediately (existing sessions follow) and is persisted
 * server-side; "reset" restores the config default (mode=null). Hidden until the
 * status has been fetched (mounted-once fetch in useAgentSession).
 */
export function ApprovalModeSwitcher() {
  const { approvalModeStatus, setApprovalMode } = useAgentSession()
  const { t } = useI18n()
  const { toast } = useNotifications()
  // 切换在途互斥：防止连点导致乱序覆盖（与原 SettingsPanel 行为一致）。
  const [busy, setBusy] = useState(false)

  if (!approvalModeStatus) return null

  const effective = approvalModeStatus.effective

  const switchTo = (mode: ApprovalModeValue | null) => {
    if (busy || mode === effective) return
    setBusy(true)
    void setApprovalMode(mode).then((ok) => {
      setBusy(false)
      if (!ok) toast({ title: t('settings.approval_fail'), severity: 'danger' })
    })
  }

  return (
    <Dropdown
      align="left"
      direction="up"
      panelClassName="min-w-[13rem]"
      trigger={
        <button
          type="button"
          aria-label={t('composer.approval_aria')}
          disabled={busy}
          className="flex h-8 min-w-0 max-w-[220px] items-center gap-1.5 rounded-lg border border-border bg-surface-2/70 px-2.5 text-xs transition-colors hover:border-border-strong hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-60"
        >
          <Icon
            name="shield"
            size={14}
            className={cn('shrink-0', effective === 'yolo' ? 'text-warning' : 'text-primary')}
          />
          <span className="truncate font-medium text-text">
            {t(MODES.find((m) => m.value === effective)?.shortKey ?? 'settings.approval_short_always_ask')}
          </span>
          <Icon name="chevron-down" size={12} className="shrink-0 text-muted" />
        </button>
      }
      items={[
        ...MODES.map((m) => ({
          label: t(m.labelKey),
          active: m.value === effective,
          disabled: busy,
          onClick: () => switchTo(m.value),
        })),
        // 运行时覆盖生效时提供「恢复配置默认」（服务端 mode=null）。
        ...(approvalModeStatus.mode
          ? [
              { divider: true },
              {
                label: t('settings.approval_reset'),
                disabled: busy,
                onClick: () => switchTo(null),
              },
            ]
          : []),
      ]}
    />
  )
}
