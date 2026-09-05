import { useCallback, useEffect, useState } from 'react'
import { BarChart } from '@/components/charts'
import { Badge, Button, EmptyState, IconButton, Spinner, useDialogA11y } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useI18n } from '@/lib/i18n'
import { compact, currency, formatNumber, percent } from '@/lib/format'
import {
  fetchStats,
  fetchTrend,
  type DailyStat,
  type ModelStat,
  type ServerStats,
  type ToolStat,
} from '@/lib/agent/stats'
import { cn } from '@/lib/cn'

/** 趋势窗口（与 /api/stats/trend 默认一致）。 */
const TREND_DAYS = 14

/**
 * 统计仪表盘：会话 / token / 成本指标卡 + 14 天趋势柱状图 + 工具调用 TOP 表 +
 * 模型用量表。数据来自 `/api/stats` 与 `/api/stats/trend`（服务端受限扫描会话
 * JSONL，见 `agent_server::collect_stats`）。趋势图复用 charts.tsx 的自绘 SVG
 * BarChart（双序列：token 主序、成本次序列，各自独立缩放），不引入新依赖。
 */
export function StatisticsPanel({ onClose }: { onClose?: () => void }) {
  const { t } = useI18n()
  const [stats, setStats] = useState<ServerStats | null>(null)
  const [trend, setTrend] = useState<DailyStat[] | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  // 全屏浮层的 dialog 语义（焦点陷阱 + Esc + 锁滚动）——此前仅 Esc，无焦点管理。
  const a11y = useDialogA11y(true, onClose ?? (() => {}))

  const load = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [s, tr] = await Promise.all([fetchStats(), fetchTrend(TREND_DAYS)])
      setStats(s)
      setTrend(tr.daily)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const usage = stats?.usage
  // 缓存命中率：命中读取占「输入侧总消耗」（非缓存输入 + 命中读取）的比例。
  const hitRate =
    usage && usage.input_tokens + usage.cache_read_tokens > 0
      ? usage.cache_read_tokens / (usage.input_tokens + usage.cache_read_tokens)
      : 0
  const totalTokens = (usage?.input_tokens ?? 0) + (usage?.output_tokens ?? 0)

  return (
    <div className="fixed inset-0 z-stats flex items-center justify-center p-4">
      <div className="app-backdrop" />
      <div
        ref={a11y.ref}
        onKeyDown={a11y.onKeyDown}
        role="dialog"
        aria-modal="true"
        aria-label={t('stats.title')}
        tabIndex={-1}
        className="relative z-10 flex h-full max-h-[88vh] w-full max-w-[1080px] flex-col overflow-hidden rounded-2xl border border-border bg-surface shadow-pop outline-none animate-scale-in"
      >
        {/* 头部 */}
        <div className="flex shrink-0 items-center justify-between gap-4 border-b border-border px-6 py-4">
          <div className="flex items-center gap-3">
            <span className="flex h-10 w-10 items-center justify-center rounded-xl bg-primary/10 text-primary">
              <Icon name="bar-chart" size={20} />
            </span>
            <div>
              <h2 className="font-display text-base font-semibold text-text">{t('stats.title')}</h2>
              <p className="text-xs text-muted">{t('stats.subtitle')}</p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="secondary" size="sm" leftIcon="refresh" onClick={load} loading={loading}>
              {t('stats.refresh')}
            </Button>
            {onClose && <IconButton icon="close" label={t('common.close')} size="sm" onClick={onClose} className="text-muted" />}
          </div>
        </div>

        {/* 主体 */}
        <div className="no-scrollbar flex-1 space-y-5 overflow-y-auto p-6">
          {loading && !stats && (
            <div className="flex items-center justify-center py-20 text-muted">
              <Spinner size={20} /> <span className="ml-2 text-sm">{t('stats.loading')}</span>
            </div>
          )}

          {error && !stats && (
            <EmptyState
              icon="alert"
              title={t('stats.error')}
              description={error}
              action={
                <Button variant="primary" leftIcon="refresh" onClick={load}>
                  {t('stats.retry')}
                </Button>
              }
            />
          )}

          {stats && (
            <>
              {/* 指标卡 */}
              <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                <MetricCard
                  icon="layers"
                  label={t('stats.sessions')}
                  value={formatNumber(stats.sessions.total)}
                  sub={t('stats.messages', { n: formatNumber(stats.sessions.total_messages) })}
                />
                <MetricCard
                  icon="sparkles"
                  label={t('stats.total_tokens')}
                  value={compact(totalTokens)}
                  sub={`${compact(usage?.input_tokens ?? 0)} → ${compact(usage?.output_tokens ?? 0)}`}
                />
                <MetricCard
                  icon="database"
                  label={t('stats.cache_hit_rate')}
                  value={percent(hitRate * 100, 1)}
                  sub={t('stats.cache_tokens', {
                    read: compact(usage?.cache_read_tokens ?? 0),
                    write: compact(usage?.cache_write_tokens ?? 0),
                  })}
                />
                <MetricCard
                  icon="dollar"
                  label={t('stats.cost')}
                  value={currency(usage?.cost_usd ?? 0)}
                  sub={t('stats.cost_estimate')}
                />
              </div>

              {/* 14 天趋势 */}
              <section className="rounded-xl border border-border bg-surface-2/40 p-4">
                <h3 className="mb-1 flex items-center gap-2 font-display text-sm font-semibold text-text">
                  <Icon name="trending-up" size={15} className="text-primary" />
                  {t('stats.trend')}
                </h3>
                {trend && trend.length > 0 ? (
                  <BarChart
                    data={trend.map((d) => ({ label: d.date.slice(5), value: d.tokens, value2: d.cost }))}
                    height={220}
                    color="rgb(var(--c-primary))"
                    color2="rgb(var(--c-accent))"
                    label2={t('stats.cost')}
                    formatValue={compact}
                  />
                ) : (
                  <p className="py-10 text-center text-sm text-muted">{t('stats.empty')}</p>
                )}
              </section>

              {/* 工具调用 TOP + 模型用量 */}
              <div className="grid gap-5 lg:grid-cols-2">
                <section className="overflow-hidden rounded-xl border border-border">
                  <header className="flex items-center gap-2 border-b border-border px-4 py-3">
                    <Icon name="zap" size={15} className="text-primary" />
                    <h3 className="font-display text-sm font-semibold text-text">{t('stats.tools')}</h3>
                  </header>
                  {stats.tools.length === 0 ? (
                    <p className="px-4 py-8 text-center text-sm text-muted">{t('stats.empty')}</p>
                  ) : (
                    <div className="max-h-72 overflow-y-auto">
                      <table className="w-full text-left text-xs">
                        <thead className="sticky top-0 bg-surface">
                          <tr className="text-muted">
                            <th className="px-4 py-2 font-medium">{t('stats.tool')}</th>
                            <th className="px-2 py-2 text-right font-medium">{t('stats.calls')}</th>
                            <th className="px-4 py-2 text-right font-medium">{t('stats.errors')}</th>
                          </tr>
                        </thead>
                        <tbody>
                          {stats.tools.map((tool: ToolStat, i) => (
                            <tr key={tool.name} className="border-t border-border/60">
                              <td className="px-4 py-2">
                                <span className="mr-2 text-muted">{i + 1}</span>
                                <span className="font-mono text-text-2">{tool.name}</span>
                              </td>
                              <td className="px-2 py-2 text-right tabular text-text-2">
                                {formatNumber(tool.calls)}
                              </td>
                              <td className="px-4 py-2 text-right">
                                {tool.errors > 0 ? (
                                  <Badge tone="danger">{formatNumber(tool.errors)}</Badge>
                                ) : (
                                  <span className="tabular text-muted">—</span>
                                )}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </section>

                <section className="overflow-hidden rounded-xl border border-border">
                  <header className="flex items-center gap-2 border-b border-border px-4 py-3">
                    <Icon name="cube" size={15} className="text-primary" />
                    <h3 className="font-display text-sm font-semibold text-text">{t('stats.models')}</h3>
                  </header>
                  {stats.top_models.length === 0 ? (
                    <p className="px-4 py-8 text-center text-sm text-muted">{t('stats.empty')}</p>
                  ) : (
                    <div className="max-h-72 overflow-y-auto">
                      <table className="w-full text-left text-xs">
                        <thead className="sticky top-0 bg-surface">
                          <tr className="text-muted">
                            <th className="px-4 py-2 font-medium">{t('stats.model')}</th>
                            <th className="px-2 py-2 text-right font-medium">{t('stats.turns')}</th>
                            <th className="px-2 py-2 text-right font-medium">{t('stats.input')}</th>
                            <th className="px-4 py-2 text-right font-medium">{t('stats.output')}</th>
                          </tr>
                        </thead>
                        <tbody>
                          {stats.top_models.map((m: ModelStat) => (
                            <tr key={m.model} className="border-t border-border/60">
                              <td className="max-w-[180px] truncate px-4 py-2 font-mono text-text-2">
                                {m.model}
                              </td>
                              <td className="px-2 py-2 text-right tabular text-text-2">
                                {formatNumber(m.turns)}
                              </td>
                              <td className="px-2 py-2 text-right tabular text-text-2">
                                {compact(m.input_tokens)}
                              </td>
                              <td className="px-4 py-2 text-right tabular text-text-2">
                                {compact(m.output_tokens)}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </section>
              </div>

              {/* 口径脚注 */}
              <p className="text-[11px] leading-relaxed text-muted">
                {t('stats.scan_note', { n: formatNumber(stats.sessions.total) })}
              </p>
            </>
          )}
        </div>
      </div>
    </div>
  )
}

/** 指标卡。 */
function MetricCard({
  icon,
  label,
  value,
  sub,
}: {
  icon: string
  label: string
  value: string
  sub?: string
}) {
  return (
    <div className="rounded-xl border border-border bg-surface-2/40 p-4">
      <div className="flex items-center gap-2 text-muted">
        <Icon name={icon} size={14} />
        <span className="text-xs">{label}</span>
      </div>
      <div className={cn('mt-1.5 font-display text-xl font-semibold tabular text-text')}>{value}</div>
      {sub && <div className="mt-0.5 truncate text-[11px] text-muted">{sub}</div>}
    </div>
  )
}
