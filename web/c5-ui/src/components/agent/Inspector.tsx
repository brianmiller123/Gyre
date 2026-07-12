import { useState } from 'react'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { SubAgentMonitor } from '@/components/agent/SubAgentMonitor'
import { useSettings } from '@/lib/settings'
import { Badge, Button, Modal, ProgressBar } from '@/components/ui'
import { Icon } from '@/components/icons'
import { stateMeta } from '@/lib/agent/ui'
import { compact, formatNumber } from '@/lib/format'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'

/** Right-hand run inspector: connection, state machine, usage, models. */
export function Inspector({ onClose }: { onClose?: () => void }) {
  const {
    state,
    usage,
    contextUsage,
    connected,
    sessionId,
    models,
    currentModel,
    stats,
    items,
    agents,
    connect,
    newChat,
    switchModel,
    compact: doCompact,
  } = useAgentSession()
  const { settings } = useSettings()
  const { t } = useI18n()
  const meta = stateMeta[state as string] ?? stateMeta.no_task
  const [pendingAlias, setPendingAlias] = useState<string | null>(null)

  // Clicking a model: switch immediately, but confirm if a conversation exists.
  const pickModel = (alias: string, isDefault: boolean) => {
    const target = isDefault ? null : alias
    if (target === (currentModel?.alias ?? null)) return
    if (items.length > 0) setPendingAlias(isDefault ? '__default__' : alias)
    else switchModel(target)
  }
  const confirmPending = () => {
    if (pendingAlias === null) return
    switchModel(pendingAlias === '__default__' ? null : pendingAlias)
    setPendingAlias(null)
  }

  const lastDone = [...items].reverse().find((i) => i.kind === 'done')
  const totalTokens = usage.input_tokens + usage.output_tokens

  return (
    <aside className="flex h-full w-full flex-col border-l border-border bg-surface/60 lg:w-80">
      <div className="flex items-center justify-between border-b border-border px-4 py-3">
        <h2 className="font-display text-sm font-semibold text-text">{t('inspector.run_panel')}</h2>
        {onClose && (
          <button onClick={onClose} className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-surface-2 hover:text-text lg:hidden">
            <Icon name="close" size={16} />
          </button>
        )}
      </div>

      <div className="no-scrollbar flex-1 space-y-4 overflow-y-auto p-4">
        {/* Connection */}
        <Section title={t('inspector.connection')} icon="wifi">
          <Row label={t('inspector.status')}>
            <Badge tone={connected ? 'success' : 'neutral'} dot={connected}>
              {connected ? t('inspector.connected') : t('inspector.disconnected')}
            </Badge>
          </Row>
          <Row label={t('inspector.server')}>
            <span className="max-w-[150px] truncate font-mono text-[11px] text-text-2">
              {settings.serverUrl || '—'}
            </span>
          </Row>
          <Row label={t('inspector.session')}>
            <span className="max-w-[150px] truncate font-mono text-[11px] text-text-2">
              {sessionId ? sessionId.slice(0, 13) + '…' : '—'}
            </span>
          </Row>
          <div className="flex gap-2 pt-1">
            <Button size="sm" variant="outline" leftIcon="refresh" className="flex-1" onClick={() => connect()}>
              {t('inspector.reconnect')}
            </Button>
            <Button size="sm" variant="ghost" leftIcon="plus" className="flex-1" onClick={() => newChat()}>
              {t('inspector.new_session')}
            </Button>
          </div>
        </Section>

        {/* State machine */}
        <Section title={t('inspector.state_machine')} icon="activity">
          <div className="flex items-center justify-between rounded-lg border border-border bg-surface-2/60 px-3 py-2.5">
            <span className="text-xs text-muted">{t('inspector.current')}</span>
            <Badge tone={meta.tone} dot={meta.dot}>
              {meta.label}
            </Badge>
          </div>
          <p className="mt-1.5 text-[11px] text-muted">{meta.desc}</p>
        </Section>

        {/* Usage */}
        <Section title={t('inspector.usage')} icon="gauge">
          <div className="mb-2 flex items-baseline justify-between">
            <span className="text-xs text-muted">{t('inspector.total_tokens')}</span>
            <span className="tabular font-display text-lg font-bold text-text">
              {compact(totalTokens)}
            </span>
          </div>
          <UsageBar label={t('inspector.input')} value={usage.input_tokens} total={totalTokens} color="rgb(var(--c-info))" />
          <UsageBar label={t('inspector.output')} value={usage.output_tokens} total={totalTokens} color="rgb(var(--c-primary))" />
          {usage.cache_read_tokens > 0 && (
            <UsageBar label={t('inspector.cache_read')} value={usage.cache_read_tokens} total={totalTokens} color="rgb(var(--c-success))" />
          )}
          <div className="mt-2 grid grid-cols-2 gap-2 text-center">
            <MiniStat label={t('inspector.cost')} value={usage.cost_usd.toFixed(4)} />
            <MiniStat label={t('inspector.last_turn')} value={lastDone && lastDone.kind === 'done' ? `${lastDone.turns} / ${lastDone.tool_calls}` : '—'} />
          </div>
        </Section>

        {/* 上下文窗口占比 + 手动压缩 */}
        {contextUsage && contextUsage.limit > 0 && (
          <Section title={t('inspector.context_window')} icon="layers">
            {(() => {
              const pct = Math.min(
                100,
                (contextUsage.current / contextUsage.limit) * 100,
              )
              return (
                <>
                  <div className="mb-2 flex items-baseline justify-between">
                    <span className="text-xs text-muted">{t('inspector.ratio')}</span>
                    <span className="tabular text-xs font-medium text-text-2">
                      {compact(contextUsage.current)} / {compact(contextUsage.limit)} tok ·{' '}
                      {pct.toFixed(1)}%
                    </span>
                  </div>
                  <ProgressBar
                    value={pct}
                    tone={pct >= 80 ? 'danger' : pct >= 50 ? 'warning' : 'primary'}
                  />
                  <Button
                    size="sm"
                    variant="outline"
                    leftIcon="layers"
                    className="mt-2 w-full"
                    onClick={() => doCompact()}
                  >
                    {t('inspector.compact')}
                  </Button>
                </>
              )
            })()}
          </Section>
        )}

        {/* Sub-agent monitoring — only while sub-agents exist */}
        {agents.length > 0 && (
          <Section title={t('inspector.subagents', { n: agents.length })} icon="activity">
            <SubAgentMonitor agents={agents} />
          </Section>
        )}

        {/* Models — click to switch (starts a new conversation) */}
        {models.length > 0 && (
          <Section title={t('inspector.models')} icon="cube">
            <ul className="space-y-1">
              {models.map((m, i) => {
                const active = currentModel?.alias === m.alias
                const isDefault = i === 0
                return (
                  <li key={m.alias}>
                    <button
                      onClick={() => pickModel(m.alias, isDefault)}
                      disabled={active}
                      className={cn(
                        'flex w-full items-center justify-between gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors',
                        active ? 'cursor-default bg-primary/10' : 'cursor-pointer hover:bg-surface-2',
                      )}
                    >
                      <span className={cn('flex min-w-0 items-center gap-1.5 font-medium', active ? 'text-primary' : 'text-text-2')}>
                        {active ? (
                          <Icon name="check" size={12} />
                        ) : (
                          <Icon name="arrow-right" size={12} className="text-muted opacity-0 group-hover:opacity-100" />
                        )}
                        <span className="truncate">{m.alias}</span>
                        {isDefault && (
                          <span className="rounded bg-surface-3 px-1 py-0 text-[9px] text-muted">{t('inspector.default_badge')}</span>
                        )}
                      </span>
                      <span className="max-w-[110px] shrink-0 truncate font-mono text-[10px] text-muted">{m.id}</span>
                    </button>
                  </li>
                )
              })}
            </ul>
            <p className="pt-1 text-[10px] text-muted">{t('inspector.switch_hint')}</p>
          </Section>
        )}

        {/* Server stats */}
        {stats && (
          <Section title={t('inspector.server_stats')} icon="server">
            <Row label={t('inspector.active_sessions')}>
              <span className="tabular text-text-2">{stats.active_sessions}</span>
            </Row>
            <Row label={t('inspector.model_count')}>
              <span className="tabular text-text-2">{stats.models_available}</span>
            </Row>
          </Section>
        )}

        <div className="rounded-lg border border-border bg-surface-2/50 p-3 text-[11px] leading-relaxed text-muted">
          <p className="mb-1 flex items-center gap-1.5 font-medium text-text-2">
            <Icon name="sparkles" size={13} className="text-primary" /> {t('inspector.tip_title')}
          </p>
          {t('inspector.tip_body')}
        </div>
      </div>

      <Modal
        open={pendingAlias !== null}
        onClose={() => setPendingAlias(null)}
        title={t('inspector.switch_model')}
        description={t('inspector.switch_model_desc')}
        icon="cube"
        size="sm"
        footer={
          <>
            <Button variant="secondary" onClick={() => setPendingAlias(null)}>
              {t('shell.cancel')}
            </Button>
            <Button variant="primary" leftIcon="check" onClick={confirmPending}>
              {t('shell.switch_and_new')}
            </Button>
          </>
        }
      >
        <p className="text-sm text-text-2">
          {t('shell.switch_confirm_body', { model: pendingAlias === '__default__' ? t('inspector.default_model') : (pendingAlias ?? '') })}
        </p>
      </Modal>
    </aside>
  )
}

function Section({ title, icon, children }: { title: string; icon: string; children: React.ReactNode }) {
  return (
    <div>
      <p className="mb-2 flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wide text-muted">
        <Icon name={icon} size={13} /> {title}
      </p>
      <div className="space-y-1.5">{children}</div>
    </div>
  )
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 text-xs">
      <span className="text-muted">{label}</span>
      {children}
    </div>
  )
}

function UsageBar({ label, value, total, color }: { label: string; value: number; total: number; color: string }) {
  const pct = total > 0 ? (value / total) * 100 : 0
  return (
    <div className="flex items-center gap-2 py-0.5">
      <span className="w-14 shrink-0 text-[11px] text-muted">{label}</span>
      <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-surface-3">
        <div className="h-full rounded-full transition-all duration-500" style={{ width: `${pct}%`, background: color }} />
      </div>
      <span className="tabular w-12 shrink-0 text-right text-[11px] text-text-2">{formatNumber(value)}</span>
    </div>
  )
}

function MiniStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border bg-surface-2/60 px-2 py-1.5">
      <div className="tabular text-sm font-semibold text-text">{value}</div>
      <div className="text-[10px] text-muted">{label}</div>
    </div>
  )
}
