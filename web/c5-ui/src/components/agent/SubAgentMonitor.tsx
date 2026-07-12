import { useState } from 'react'
import { Badge, ProgressBar, type Tone } from '@/components/ui'
import { Icon } from '@/components/icons'
import { compact, formatNumber } from '@/lib/format'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'
import type { SubAgentLogLine, SubAgentPhase, SubAgentStatus } from '@/lib/agent/types'

/** 子 Agent 阶段 → 徽标（tone / 文案 / 脉冲）。 */
const PHASE_META: Record<SubAgentPhase, { labelKey: string; tone: Tone; pulse?: boolean }> = {
  pending: { labelKey: 'monitor.pending', tone: 'neutral' },
  running: { labelKey: 'monitor.running', tone: 'primary', pulse: true },
  streaming: { labelKey: 'monitor.streaming', tone: 'info', pulse: true },
  waiting_tool: { labelKey: 'monitor.waiting_tool', tone: 'warning' },
  done: { labelKey: 'monitor.done', tone: 'success' },
  failed: { labelKey: 'monitor.failed', tone: 'danger' },
  cancelled: { labelKey: 'monitor.cancelled', tone: 'neutral' },
}

/** 日志级别 → 颜色类。 */
const LOG_COLOR: Record<SubAgentLogLine['level'], string> = {
  info: 'text-text-2',
  debug: 'text-muted',
  warn: 'text-warning',
  error: 'text-danger',
}

function formatDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m${s % 60}s`
  const h = Math.floor(m / 60)
  return `${h}h${m % 60}m`
}

/**
 * 子 Agent 实时监控面板：卡片网格展示每个子 Agent 的阶段、进度、资源消耗与日志流。
 * 数据来自 `ServerFrame::sub_agents` 聚合帧（≈8fps 整体替换）。
 */
export function SubAgentMonitor({ agents }: { agents: SubAgentStatus[] }) {
  const { t } = useI18n()
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const running = agents.filter((a) => !isTerminal(a.phase)).length
  const done = agents.filter((a) => a.phase === 'done').length
  const failed = agents.filter((a) => a.phase === 'failed').length
  const totalTokens = agents.reduce(
    (acc, a) => acc + a.usage.input_tokens + a.usage.output_tokens,
    0,
  )

  return (
    <div className="space-y-2">
      {/* 汇总条 */}
      <div className="flex items-center justify-between rounded-lg border border-border bg-surface-2/60 px-2.5 py-1.5 text-[11px]">
        <span className="text-muted">
          {t('monitor.active')} <span className="tabular font-semibold text-text">{running}</span> · {t('monitor.done')}{' '}
          <span className="tabular font-semibold text-success">{done}</span> · {t('monitor.failed')}{' '}
          <span className="tabular font-semibold text-danger">{failed}</span>
        </span>
        <span className="tabular text-text-2">{compact(totalTokens)} tok</span>
      </div>

      {/* 卡片列表 */}
      {agents.map((a) => {
        const meta = PHASE_META[a.phase]
        const tokens = a.usage.input_tokens + a.usage.output_tokens
        const isOpen = expanded.has(a.id)
        const showLogs = a.logs.length > 0
        return (
          <div
            key={a.id}
            className="rounded-lg border border-border bg-surface-2/40 p-2.5 transition-colors hover:border-border/80"
          >
            {/* 头：阶段 + 标签 */}
            <div className="mb-1.5 flex items-center gap-2">
              <Badge tone={meta.tone} dot pulse={meta.pulse}>
                {t(meta.labelKey)}
              </Badge>
              <span className="min-w-0 flex-1 truncate text-xs font-medium text-text" title={a.task}>
                {a.label}
              </span>
              <span className="shrink-0 font-mono text-[10px] text-muted">
                {formatDuration(a.updated_at - a.started_at)}
              </span>
            </div>

            {/* 进度条 */}
            <ProgressBar
              value={Math.round(a.progress * 100)}
              tone={a.phase === 'failed' ? 'danger' : meta.tone}
              size="sm"
            />

            {/* 资源消耗 */}
            <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[10px] text-muted">
              <span>
                {t('monitor.turns')} <span className="tabular text-text-2">{a.turns}</span>
              </span>
              <span>
                {t('monitor.tools')} <span className="tabular text-text-2">{a.tool_calls}</span>
              </span>
              <span>
                tokens <span className="tabular text-text-2">{formatNumber(tokens)}</span>
              </span>
              {a.usage.cost_usd > 0 && (
                <span>
                  ${a.usage.cost_usd.toFixed(4)}
                </span>
              )}
              {a.current_activity && (
                <span className="flex min-w-0 items-center gap-1 text-info">
                  <Icon name="activity" size={10} />
                  <span className="truncate">{a.current_activity}</span>
                </span>
              )}
            </div>

            {/* 错误 */}
            {a.error && (
              <p className="mt-1.5 flex items-start gap-1 text-[10px] text-danger">
                <Icon name="alert" size={11} className="mt-px shrink-0" />
                <span className="break-all">{a.error}</span>
              </p>
            )}

            {/* 日志流（可折叠） */}
            {showLogs && (
              <div className="mt-1.5">
                <button
                  onClick={() => toggle(a.id)}
                  className="flex items-center gap-1 text-[10px] text-muted transition-colors hover:text-text-2"
                >
                  <Icon name={isOpen ? 'chevron-down' : 'chevron-right'} size={11} />
                  日志（{a.logs.length}）
                </button>
                {isOpen && (
                  <pre className="mt-1 max-h-32 overflow-y-auto rounded bg-surface-3/60 p-1.5 font-mono text-[10px] leading-relaxed">
                    {a.logs.map((l, i) => (
                      <div key={i} className={cn('whitespace-pre-wrap break-all', LOG_COLOR[l.level])}>
                        {l.text}
                      </div>
                    ))}
                  </pre>
                )}
              </div>
            )}
          </div>
        )
      })}
    </div>
  )
}

function isTerminal(p: SubAgentPhase): boolean {
  return p === 'done' || p === 'failed' || p === 'cancelled'
}
