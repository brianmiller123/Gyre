import { useAgentSession } from '@/lib/agent/useAgentSession'
import { SubAgentMonitor } from '@/components/agent/SubAgentMonitor'
import { Badge, Button, IconButton, ProgressBar, SectionLabel } from '@/components/ui'
import { Icon } from '@/components/icons'
import { stateMeta } from '@/lib/agent/ui'
import { compact, formatNumber } from '@/lib/format'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'

/**
 * 右侧运行面板：状态机 / 用量 / 上下文 / 子代理 / 服务端统计。
 *
 * 定位（信息架构）：这是**遥测详情**的唯一归属地。顶栏那枚遥测胶囊只提供总量
 * 摘要并负责开合本面板——两者是"摘要 → 详情"的层级关系，而非同一份读数的两份
 * 拷贝。连接信息同样只在这里给"运行中"所需的最小集（状态 + 会话 id + 重连），
 * 服务器地址属于偏好，归设置面板。
 */
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
    lastDone,
    agents,
    connect,
    compact: doCompact,
  } = useAgentSession()
  const { t } = useI18n()
  const meta = stateMeta[state as string] ?? stateMeta.no_task

  // lastDone 由 provider 记忆化（done 边界才换身份）：本组件流式期间不再随 items 重渲染。
  const totalTokens = usage.input_tokens + usage.output_tokens

  return (
    <aside className="glass-bar flex h-full w-80 max-w-full flex-col border-l">
      <div className="flex h-14 shrink-0 items-center justify-between border-b border-border px-4">
        <h2 className="font-display text-md font-semibold text-text">{t('inspector.run_panel')}</h2>
        {onClose && (
          <IconButton
            icon="close"
            label={t('common.close')}
            size="sm"
            onClick={onClose}
            className="-mr-1 text-muted"
          />
        )}
      </div>

      <div className="no-scrollbar flex-1 space-y-5 overflow-y-auto p-4">
        {/* 连接：只保留运行期必需的最小集（状态 / 会话 / 重连）。
            断线重连必须 resume 当前会话：裸 connect() 会新建会话，导致本地
            transcript 与服务端日志行号错位。 */}
        <Section title={t('inspector.connection')} icon="wifi">
          <div className="card-inset flex items-center justify-between gap-2 px-3 py-2">
            <Badge tone={connected ? 'success' : 'neutral'} dot={connected}>
              {connected ? t('inspector.connected') : t('inspector.disconnected')}
            </Badge>
            <span className="truncate font-mono text-2xs text-muted">
              {sessionId ? sessionId.slice(0, 13) + '…' : '—'}
            </span>
          </div>
          <Button
            size="sm"
            variant="outline"
            leftIcon="refresh"
            className="w-full"
            onClick={() => connect(sessionId)}
          >
            {t('inspector.reconnect')}
          </Button>
        </Section>

        {/* 状态机 */}
        <Section title={t('inspector.state_machine')} icon="activity">
          <div className="card-inset flex items-center justify-between px-3 py-2.5">
            <span className="text-xs text-muted">{t('inspector.current')}</span>
            <Badge tone={meta.tone} dot={meta.dot}>
              {t(meta.label)}
            </Badge>
          </div>
          <p className="text-2xs text-muted">{t(meta.desc)}</p>
        </Section>

        {/* 用量 */}
        <Section title={t('inspector.usage')} icon="gauge">
          <div className="flex items-baseline justify-between">
            <span className="text-xs text-muted">{t('inspector.total_tokens')}</span>
            <span className="tabular font-display text-xl font-bold text-text">
              {compact(totalTokens)}
            </span>
          </div>
          <UsageBar
            label={t('inspector.input')}
            value={usage.input_tokens}
            total={totalTokens}
            color="rgb(var(--c-info))"
          />
          <UsageBar
            label={t('inspector.output')}
            value={usage.output_tokens}
            total={totalTokens}
            color="rgb(var(--c-primary))"
          />
          {usage.cache_read_tokens > 0 && (
            <UsageBar
              label={t('inspector.cache_read')}
              value={usage.cache_read_tokens}
              total={totalTokens}
              color="rgb(var(--c-success))"
            />
          )}
          <div className="grid grid-cols-2 gap-2 text-center">
            <MiniStat label={t('inspector.cost')} value={usage.cost_usd.toFixed(4)} />
            <MiniStat
              label={t('inspector.last_turn')}
              value={
                lastDone && lastDone.kind === 'done'
                  ? `${lastDone.turns} / ${lastDone.tool_calls}`
                  : '—'
              }
            />
          </div>
        </Section>

        {/* 上下文窗口占比 + 手动压缩 */}
        {contextUsage && contextUsage.limit > 0 && (
          <Section title={t('inspector.context_window')} icon="layers">
            {(() => {
              const pct = Math.min(100, (contextUsage.current / contextUsage.limit) * 100)
              return (
                <>
                  <div className="flex items-baseline justify-between">
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
                    className="w-full"
                    onClick={() => doCompact()}
                  >
                    {t('inspector.compact')}
                  </Button>
                </>
              )
            })()}
          </Section>
        )}

        {/* 子代理监控 —— 仅在有子代理时出现 */}
        {agents.length > 0 && (
          <Section title={t('inspector.subagents', { n: agents.length })} icon="activity">
            <SubAgentMonitor agents={agents} />
          </Section>
        )}

        {/* 模型 —— 只读清单；切换入口是输入区上方的模型胶囊 */}
        {models.length > 0 && (
          <Section title={t('inspector.models')} icon="cube">
            <ul className="space-y-1">
              {models.map((m, i) => {
                const active = currentModel?.alias === m.alias
                const isDefault = i === 0
                return (
                  <li
                    key={m.alias}
                    className={cn(
                      'flex items-center justify-between gap-2 rounded-md px-2 py-1.5 text-xs',
                      active ? 'bg-primary/10' : 'bg-surface-2',
                    )}
                  >
                    <span
                      className={cn(
                        'flex min-w-0 items-center gap-1.5 font-medium',
                        active ? 'text-primary' : 'text-text-2',
                      )}
                    >
                      {active && <Icon name="check" size={12} className="shrink-0" />}
                      <span className="truncate">{m.alias}</span>
                      {isDefault && (
                        <span className="rounded-sm bg-surface-3 px-1 text-2xs text-muted">
                          {t('inspector.default_badge')}
                        </span>
                      )}
                    </span>
                    <span className="max-w-[110px] shrink-0 truncate font-mono text-2xs text-muted">
                      {m.id}
                    </span>
                  </li>
                )
              })}
            </ul>
          </Section>
        )}

        {/* 服务端统计 */}
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

        <div className="card-inset p-3 text-2xs leading-relaxed text-muted">
          <p className="mb-1 flex items-center gap-1.5 font-medium text-text-2">
            <Icon name="sparkles" size={14} className="text-primary" /> {t('inspector.tip_title')}
          </p>
          {t('inspector.tip_body')}
        </div>
      </div>
    </aside>
  )
}

function Section({
  title,
  icon,
  children,
}: {
  title: string
  icon: string
  children: React.ReactNode
}) {
  return (
    <section>
      <SectionLabel icon={icon} className="mb-2">
        {title}
      </SectionLabel>
      <div className="space-y-1.5">{children}</div>
    </section>
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

function UsageBar({
  label,
  value,
  total,
  color,
}: {
  label: string
  value: number
  total: number
  color: string
}) {
  const pct = total > 0 ? (value / total) * 100 : 0
  return (
    <div className="flex items-center gap-2 py-0.5">
      <span className="w-14 shrink-0 text-2xs text-muted">{label}</span>
      <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-surface-3">
        <div
          className="h-full rounded-full transition-[width] duration-500"
          style={{ width: `${pct}%`, background: color }}
        />
      </div>
      <span className="tabular w-12 shrink-0 text-right text-2xs text-text-2">
        {formatNumber(value)}
      </span>
    </div>
  )
}

function MiniStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="card-inset px-2 py-1.5">
      <div className="tabular text-sm font-semibold text-text">{value}</div>
      <div className="text-2xs text-muted">{label}</div>
    </div>
  )
}
