/** 统计端点类型与 fetch 助手（`/api/stats` 与 `/api/stats/trend`，镜像 agent_server::StatsAgg）。 */
import type { Usage } from '@/lib/agent/types'

/** 会话统计摘要。 */
export interface SessionsSummary {
  total: number
  total_messages: number
}

/** 单工具聚合。 */
export interface ToolStat {
  name: string
  calls: number
  errors: number
}

/** 单模型聚合。 */
export interface ModelStat {
  model: string
  turns: number
  input_tokens: number
  output_tokens: number
}

/** 单日聚合（`date` 为 `YYYY-MM-DD`）。 */
export interface DailyStat {
  date: string
  tokens: number
  cost: number
}

/** `/api/stats` 完整载荷。 */
export interface ServerStats {
  active_sessions: number
  models_available: number
  sessions: SessionsSummary
  usage: Usage
  tools: ToolStat[]
  top_models: ModelStat[]
  daily: DailyStat[]
}

/** `/api/stats/trend?days=14` 载荷。 */
export interface TrendResponse {
  days: number
  daily: DailyStat[]
}

/**
 * 连接上下文：由 agent-session provider 在挂载 / 设置变更时绑定（与
 * `lib/agent/workspace.ts` 同一套绑定时机），fetch 助手据此携带鉴权 token。
 */
let boundOrigin = ''
let boundToken = ''

export function bindStatsContext(serverUrl: string, token: string) {
  boundOrigin = serverUrl.replace(/\/$/, '')
  boundToken = token
}

function withToken(extra: Record<string, string>): string {
  const params = new URLSearchParams(extra)
  if (boundToken) params.set('token', boundToken)
  return `?${params.toString()}`
}

/** 拉取运行时统计（会话 / usage / 工具 / 模型 / 按日聚合）。 */
export async function fetchStats(): Promise<ServerStats> {
  const r = await fetch(`${boundOrigin}/api/stats${withToken({})}`)
  if (!r.ok) throw new Error(`stats (HTTP ${r.status})`)
  return r.json()
}

/** 拉取最近 `days` 天的趋势（与 `/api/stats` 的 daily 同源）。 */
export async function fetchTrend(days = 14): Promise<TrendResponse> {
  const r = await fetch(`${boundOrigin}/api/stats/trend${withToken({ days: String(days) })}`)
  if (!r.ok) throw new Error(`stats/trend (HTTP ${r.status})`)
  return r.json()
}
