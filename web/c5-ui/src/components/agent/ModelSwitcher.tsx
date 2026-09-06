import { useState } from 'react'
import { Button, Dropdown, Modal } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useI18n } from '@/lib/i18n'

/** Sentinel for "switch back to the server default" (sent as model=null). */
const DEFAULT_ALIAS = '__default__'

/**
 * Compact model switcher for the composer toolbar (above the input box).
 * Index 0 in `models` is the server default (sent as model=null). Switching is
 * session-bound: with an existing conversation it asks for confirmation first,
 * because it starts a fresh session.
 */
export function ModelSwitcher() {
  // hasTranscript 是 items.length>0 的低频派生布尔：不订阅 items 本身，流式期间不重渲染。
  const { models, currentModel, hasTranscript, running, switchModel } = useAgentSession()
  const { t } = useI18n()
  // null = closed; otherwise the pending alias (DEFAULT_ALIAS for the server default).
  const [pending, setPending] = useState<string | null>(null)

  if (models.length === 0) return null

  const currentAlias = currentModel?.alias ?? null

  // Re-picking the active model is a no-op; with an existing conversation, confirm first.
  const pick = (alias: string | null) => {
    if (alias === currentAlias) return
    if (hasTranscript) setPending(alias ?? DEFAULT_ALIAS)
    else switchModel(alias)
  }

  return (
    <>
      <Dropdown
        align="left"
        direction="up"
        panelClassName="min-w-[15rem]"
        trigger={
          <button
            type="button"
            aria-label={t('shell.switch_model')}
            className="flex h-8 min-w-0 max-w-[220px] items-center gap-1.5 rounded-lg border border-border bg-surface-2/70 px-2.5 text-xs transition-colors hover:border-border-strong hover:bg-surface-2 sm:max-w-[260px]"
          >
            <Icon name="cube" size={14} className="shrink-0 text-primary" />
            <span className="truncate font-medium text-text">
              {currentAlias ?? t('shell.default_model')}
            </span>
            <Icon name="chevron-down" size={12} className="shrink-0 text-muted" />
          </button>
        }
        items={models.map((m, i) => ({
          label: `${m.alias}  ·  ${m.id}`,
          active: currentAlias === m.alias,
          onClick: () => pick(i === 0 ? null : m.alias),
        }))}
      />

      <Modal
        open={pending !== null}
        onClose={() => setPending(null)}
        title={t('shell.switch_model')}
        description={t('shell.switch_model_desc')}
        icon="cube"
        size="sm"
        footer={
          <>
            <Button variant="secondary" onClick={() => setPending(null)}>
              {t('shell.cancel')}
            </Button>
            <Button
              variant="primary"
              leftIcon="check"
              onClick={() => {
                if (pending !== null) switchModel(pending === DEFAULT_ALIAS ? null : pending)
                setPending(null)
              }}
            >
              {t('shell.switch_and_new')}
            </Button>
          </>
        }
      >
        <p className="text-sm text-text-2">
          {t('shell.switch_confirm_body', {
            model: pending === DEFAULT_ALIAS ? t('shell.default') : (pending ?? ''),
          })}
        </p>
        {running && (
          <p className="mt-2 flex items-start gap-1.5 rounded-lg bg-warning/10 px-2.5 py-2 text-xs font-medium text-warning">
            <Icon name="alert" size={14} className="mt-0.5 shrink-0" />
            {t('shell.switch_running_warn')}
          </p>
        )}
      </Modal>
    </>
  )
}
