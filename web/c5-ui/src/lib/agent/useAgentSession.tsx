import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import type { ReactNode } from 'react'
import {
  parseFrame,
  type AgentStateName,
  type AskResponseValue,
  type BranchTree,
  type ClientFrame,
  type CollabRoom,
  type ContentInput,
  type CustomCommandInfo,
  type McpToolInfo,
  type ModelInfo,
  type SessionHistoryItem,
  type SessionListItem,
  type SessionOpResult,
  type SkillInfo,
  type SubAgentStatus,
  type TranscriptItem,
  type Usage,
} from '@/lib/agent/types'
import { bindWorkspaceContext } from '@/lib/agent/workspace'
import { useSettings } from '@/lib/settings'
import { useI18n } from '@/lib/i18n'
import type { Mode } from '@/lib/settings'

interface Stats {
  active_sessions: number
  models_available: number
}

interface AgentSessionValue {
  items: TranscriptItem[]
  state: AgentStateName | string
  usage: Usage
  /** 上下文窗口 token 占比（current / limit），来自 ServerFrame::ContextUsage。 */
  contextUsage: { current: number; limit: number } | null
  running: boolean
  /** 正在请求停止（点击停止后的过渡态，给 UI 即时反馈；后端确认 / 断连后自动清除）。 */
  stopping: boolean
  connected: boolean
  connecting: boolean
  error: string | null
  sessionId: string | null
  models: ModelInfo[]
  currentModel: ModelInfo | null
  stats: Stats | null
  /** 子 Agent 实时监控快照（来自 ServerFrame::sub_agents 聚合帧）。 */
  agents: SubAgentStatus[]
  /** 历史会话列表（含首条用户输入预览，侧栏切换器用）。 */
  sessions: SessionListItem[]
  /** 历史会话列表是否正在加载（首次/手动刷新）。 */
  sessionsLoading: boolean
  /** 历史会话列表加载错误（网络异常等），UI 据此展示重试。 */
  sessionsError: string | null
  connect: (resume?: string | null, fork?: string | null) => void
  disconnect: () => void
  newChat: () => void
  /** 恢复到指定历史会话（拉取历史填充对话 + 重连 resume=id）。 */
  switchSession: (id: string) => void
  /** 刷新历史会话列表。 */
  refreshSessions: () => void
  /** 删除指定历史会话（落盘文件 + 标题）。若删除当前会话则自动新建。 */
  deleteSession: (id: string) => Promise<SessionOpResult>
  /** 重命名指定历史会话（设置自定义标题）。 */
  renameSession: (id: string, title: string) => Promise<SessionOpResult>
  /** 删除指定对话项（user 输入或 assistant 响应），落盘并就地刷新 transcript。 */
  deleteMessage: (item: TranscriptItem) => Promise<SessionOpResult>
  switchModel: (alias: string | null) => void
  /** 切换模式：以 resume=当前会话 重连（应用新 system prompt，保留对话历史）。 */
  switchMode: (mode: Mode) => void
  send: (text: string) => void
  /** 发送带多模态内容块（图片等）的消息。 */
  sendContent: (text: string, content: ContentInput[]) => void
  /** 手动压缩上下文（对应 CLI `/compact`）。 */
  compact: () => void
  /** 复制当前会话为新 id 后继续（对应 CLI `--fork`）。 */
  forkSession: () => void
  respond: (askId: string, response: AskResponseValue) => void
  cancel: () => void
  clear: () => void
  /** Push a local informational line into the transcript (not sent to the agent). */
  say: (text: string, level?: string) => void
  /** 拉取自定义 slash 命令（`/api/commands`）。 */
  fetchCustomCommands: () => Promise<CustomCommandInfo[]>
  /** 拉取已加载 Skill 列表（`/api/sessions/{id}/skills`）。 */
  fetchSkills: () => Promise<SkillInfo[]>
  /** 拉取指定 Skill 正文（`/api/sessions/{id}/skill/{name}`）。 */
  fetchSkillBody: (name: string) => Promise<string | null>
  /** 拉取已加载 MCP 工具列表（`/api/sessions/{id}/mcp`）。 */
  fetchMcp: () => Promise<McpToolInfo[]>
  /** 拉取指定会话的分支树（`/api/sessions/{id}/branches`）。 */
  fetchBranches: (sessionId: string) => Promise<BranchTree | null>
  /** 切换活跃分支（活跃会话即时生效并重载 transcript；`handoff` 注入被离开分支的摘要）。 */
  switchBranch: (leafId: string, handoff?: boolean) => Promise<SessionOpResult>
  /** 生成端到端加密协同房间（`/api/collab/room`）。 */
  newCollabRoom: () => Promise<CollabRoom | null>
}

const AgentSessionContext = createContext<AgentSessionValue | null>(null)

type NewTranscriptItem = {
  [K in TranscriptItem['kind']]: Omit<Extract<TranscriptItem, { kind: K }>, 'id' | 'ts'>
}[TranscriptItem['kind']]

const EMPTY_USAGE: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  cost_usd: 0,
}

function addUsage(a: Usage, b: Usage): Usage {
  return {
    input_tokens: a.input_tokens + b.input_tokens,
    output_tokens: a.output_tokens + b.output_tokens,
    cache_read_tokens: a.cache_read_tokens + b.cache_read_tokens,
    cache_write_tokens: a.cache_write_tokens + b.cache_write_tokens,
    cost_usd: a.cost_usd + b.cost_usd,
  }
}

/**
 * 按顺序把历史项（user/thinking/assistant）与当前 transcript 中的同类项配对，
 * 返回指定 id 项对应的日志行索引。
 *
 * 删除仅在 idle 时触发，此时实时项已全部落盘，transcript 中可删除项与历史项
 * 顺序一一对应；遇到 streaming（未落盘）的项会因顺序错位而无法匹配，返回 undefined。
 */
function resolveLineIndex(
  items: TranscriptItem[],
  itemId: string,
  history: SessionHistoryItem[],
): number | undefined {
  let hi = 0
  for (const it of items) {
    if (it.kind !== 'user' && it.kind !== 'assistant' && it.kind !== 'thinking') continue
    const h = history[hi++]
    if (it.id === itemId) return h?.line
  }
  return undefined
}

/**
 * Owns the connection to `agent --serve`: creates a session over HTTP, opens a
 * WebSocket, and reduces the streamed `ServerFrame`s into a renderable
 * transcript. Sends `ClientFrame`s for new tasks, approvals and cancellation.
 */
export function AgentSessionProvider({ children }: { children: ReactNode }) {
  const { settings, update } = useSettings()
  const { t } = useI18n()
  // i18n 经 ref 注入 WS 闭包，确保语言切换后错误提示即时跟随最新语言。
  const tRef = useRef(t)
  tRef.current = t
  const settingsRef = useRef(settings)
  settingsRef.current = settings
  // Keep the workspace API client pointed at the live server + token.
  useEffect(() => {
    bindWorkspaceContext(settings.serverUrl, settings.token)
  }, [settings.serverUrl, settings.token])
  const [items, setItems] = useState<TranscriptItem[]>([])
  // items 真值引用：deleteMessage 解析行索引时读取最新列表，避免闭包捕获陈旧快照。
  const itemsRef = useRef<TranscriptItem[]>([])
  itemsRef.current = items
  const [state, setState] = useState<AgentStateName | string>('no_task')
  const [usage, setUsage] = useState<Usage>(EMPTY_USAGE)
  const [connected, setConnected] = useState(false)
  const [connecting, setConnecting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [sessionId, setSessionId] = useState<string | null>(null)
  const [models, setModels] = useState<ModelInfo[]>([])
  const [stats, setStats] = useState<Stats | null>(null)
  const [agents, setAgents] = useState<SubAgentStatus[]>([])
  const [sessions, setSessions] = useState<SessionListItem[]>([])
  const [contextUsage, setContextUsage] = useState<{ current: number; limit: number } | null>(null)
  // 停止过渡态：点击「停止」后置 true，给输入框 / 顶栏按钮即时反馈；running 或 connected
  // 翻为 false 时由下方 effect 自动复位。
  const [stopping, setStopping] = useState(false)
  // 历史会话列表的加载/错误状态（驱动侧栏骨架屏与重试 UI）。
  const [sessionsLoading, setSessionsLoading] = useState(false)
  const [sessionsError, setSessionsError] = useState<string | null>(null)
  // sessionId 的真值引用：deleteSession 读取避免闭包捕获陈旧 id。
  const sessionIdRef = useRef<string | null>(null)
  sessionIdRef.current = sessionId

  const wsRef = useRef<WebSocket | null>(null)
  const openRef = useRef<{ assistant?: string; thinking?: string }>({})
  const idCounter = useRef(0)
  const intentionalClose = useRef(false)
  // 连接状态「真值」：用 ref 而非闭包变量，避免 newChat/重连的 setTimeout 捕获陈旧状态而提前 return。
  const connectingRef = useRef(false)
  const connectedRef = useRef(false)
  // 连接代次：屏蔽被取代的旧 socket 的迟到回调，防止其污染新连接状态（切换会话的核心修复）。
  const genRef = useRef(0)
  // 已处理过的 ask.id 集合，用于去重（防止批准框显示多次）。
  const seenAskIds = useRef<Set<string>>(new Set())
  // 心跳看门狗：记录最后一次收到服务端帧的时间戳，用于检测后端静默终止。
  const lastActivityRef = useRef<number>(Date.now())
  // WebSocket 重连退避计数（连接成功后重置为 0）。
  const reconnectAttemptsRef = useRef(0)
  // 心跳看门狗定时器 id。
  const heartbeatTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  const uid = () => `m${Date.now()}-${idCounter.current++}`

  const running = state === 'running' || state === 'streaming' || state === 'waiting_for_input'

  // stopping 仅在「运行中且已连接」时保留：任务结束（state 翻为非 running）或连接断开时
  // 自动复位，避免网络抖动 / 意外断连导致按钮永久卡在「停止中…」过渡态。
  useEffect(() => {
    if (!running || !connected) setStopping(false)
  }, [running, connected])

  // 心跳看门狗：running 状态下若超过 60 秒未收到任何服务端帧，判定后端已静默终止，
  // 自动重置状态为 idle 并提示用户，防止 UI 永久卡死在「生成中」。
  useEffect(() => {
    if (!running || !connected) {
      if (heartbeatTimerRef.current) {
        clearTimeout(heartbeatTimerRef.current)
        heartbeatTimerRef.current = null
      }
      return
    }
    const CHECK_INTERVAL = 10_000 // 每 10 秒检查一次
    const STALE_THRESHOLD = 60_000 // 60 秒无活动视为静默终止
    const tick = () => {
      const elapsed = Date.now() - lastActivityRef.current
      if (elapsed >= STALE_THRESHOLD) {
        // 后端静默终止：注入提示并重置状态
        setItems((prev) => [
          ...prev,
          {
            id: uid(),
            kind: 'say',
            text: '⚠️ 检测到服务端超过 60 秒无响应，任务可能已异常终止，状态已自动重置',
            level: 'warning',
            ts: Date.now(),
          } as TranscriptItem,
        ])
        setState('idle')
        heartbeatTimerRef.current = null
        return
      }
      heartbeatTimerRef.current = setTimeout(tick, CHECK_INTERVAL)
    }
    heartbeatTimerRef.current = setTimeout(tick, CHECK_INTERVAL)
    return () => {
      if (heartbeatTimerRef.current) {
        clearTimeout(heartbeatTimerRef.current)
        heartbeatTimerRef.current = null
      }
    }
  }, [running, connected])

  // 每次 render 把最新连接状态同步到 ref（供 connect 的 guard 与回调读取真值）。
  connectingRef.current = connecting
  connectedRef.current = connected

  const closeOpen = useCallback((which: 'assistant' | 'thinking') => {
    const id = openRef.current[which]
    if (!id) return
    openRef.current[which] = undefined
    setItems((prev) =>
      prev.map((it) => (it.id === id && 'streaming' in it ? { ...it, streaming: false } : it)),
    )
  }, [])

  const appendDelta = useCallback((which: 'assistant' | 'thinking', delta: string) => {
    let id = openRef.current[which]
    if (!id) {
      id = uid()
      openRef.current[which] = id
      setItems((prev) => [
        ...prev,
        { id, kind: which, text: delta, ts: Date.now(), streaming: true } as TranscriptItem,
      ])
    } else {
      setItems((prev) =>
        prev.map((it) => (it.id === id && 'text' in it ? { ...it, text: it.text + delta } : it)),
      )
    }
  }, [])

  const pushItem = useCallback((item: NewTranscriptItem) => {
    setItems((prev) => [...prev, { ...item, id: uid(), ts: Date.now() } as TranscriptItem])
  }, [])

  const disconnect = useCallback(() => {
    intentionalClose.current = true
    genRef.current++ // 使任何在飞的旧 socket 回调立即失效
    wsRef.current?.close()
    wsRef.current = null
    // 关键：重置 connecting，否则 newChat 的 setTimeout(connect) 会因 guard 提前返回、永远不再重连。
    connectingRef.current = false
    connectedRef.current = false
    setConnecting(false)
    setConnected(false)
    setSessionId(null)
    setState('no_task')
    openRef.current = {}
  }, [])

  /** 拉取历史会话列表（含首条用户输入预览 + 自定义标题，用于侧栏切换器）。 */
  const refreshSessions = useCallback(async () => {
    const cfg = settingsRef.current
    const origin = cfg.serverUrl.replace(/\/$/, '')
    const qs = new URLSearchParams()
    if (cfg.token) qs.set('token', cfg.token)
    setSessionsLoading(true)
    setSessionsError(null)
    try {
      const r = await fetch(`${origin}/api/sessions/list?${qs}`)
      if (!r.ok) throw new Error(`HTTP ${r.status}`)
      const d = await r.json()
      if (d && Array.isArray(d.sessions)) setSessions(d.sessions as SessionListItem[])
    } catch (e) {
      setSessionsError(e instanceof Error ? e.message : String(e))
    } finally {
      setSessionsLoading(false)
    }
  }, [])

  /** 拉取某会话的历史消息列表（含日志行索引 line）。 */
  const fetchHistoryList = useCallback(
    async (id: string): Promise<SessionHistoryItem[]> => {
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const qs = new URLSearchParams()
      if (cfg.token) qs.set('token', cfg.token)
      try {
        const res = await fetch(`${origin}/api/sessions/${encodeURIComponent(id)}/history?${qs}`)
        if (!res.ok) return []
        const data = await res.json()
        return Array.isArray(data.items) ? (data.items as SessionHistoryItem[]) : []
      } catch {
        return []
      }
    },
    [],
  )

  /** 拉取某会话的历史消息并填充到对话区（恢复展示用）。历史项携带日志行索引 `line`，
   *  供前端定位删除目标。 */
  const loadHistory = useCallback(
    async (id: string) => {
      const list = await fetchHistoryList(id)
      const mapped: TranscriptItem[] = list.map((h) =>
        h.kind === 'user'
          ? { id: uid(), kind: 'user', text: h.text, ts: Date.now(), line: h.line }
          : h.kind === 'thinking'
            ? { id: uid(), kind: 'thinking', text: h.text, ts: Date.now(), line: h.line }
            : { id: uid(), kind: 'assistant', text: h.text, ts: Date.now(), line: h.line },
      )
      setItems(mapped)
    },
    [fetchHistoryList],
  )

  const connect = useCallback(async (resume?: string | null, fork?: string | null) => {
    // 用 ref 作真值：newChat/重连的 setTimeout 即使捕获陈旧闭包，也能读到最新连接态。
    if (connectingRef.current || connectedRef.current) return
    connectingRef.current = true
    setConnecting(true)
    setError(null)
    intentionalClose.current = false
    // 连接代次：屏蔽被取代的旧 socket 的迟到回调，防止其污染新连接状态。
    const gen = ++genRef.current
    try {
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const qs = new URLSearchParams()
      if (cfg.token) qs.set('token', cfg.token)
      if (cfg.model) qs.set('model', cfg.model)
      if (cfg.mode) qs.set('mode', cfg.mode)
      if (resume) qs.set('resume', resume)
      if (fork) qs.set('fork', fork)
      const sessionQuery = qs.toString()
      const tokenOnly = cfg.token ? `?token=${encodeURIComponent(cfg.token)}` : ''
      const res = await fetch(`${origin}/api/sessions${sessionQuery ? `?${sessionQuery}` : ''}`, {
        method: 'GET',
      })
      if (!res.ok) throw new Error(`创建会话失败 (HTTP ${res.status})`)
      const data = await res.json()
      const sid: string = data.session_id
      const wsPath: string = data.ws_url ?? `/ws/${sid}`
      const urlObj = new URL(origin)
      const proto = urlObj.protocol === 'https:' ? 'wss' : 'ws'
      const wsUrl = `${proto}://${urlObj.host}${wsPath}${tokenOnly}`

      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      // 仅当本 socket 仍是当前连接（代次未变且未被取代）时才应用状态变更。
      const mine = () => genRef.current === gen && wsRef.current === ws
      ws.onopen = () => {
        if (!mine()) return
        connectedRef.current = true
        connectingRef.current = false
        reconnectAttemptsRef.current = 0 // 连接成功，重置重连退避计数
        setConnected(true)
        setConnecting(false)
        setSessionId(sid)
        setState('no_task')
        setContextUsage(null)
        // best-effort metadata
        fetch(`${origin}/api/models`).then((r) => r.json()).then(setModels).catch(() => {})
        fetch(`${origin}/api/stats${tokenOnly}`).then((r) => r.json()).then(setStats).catch(() => {})
        refreshSessions()
      }
      ws.onclose = () => {
        if (!mine()) return // 旧 socket 迟到的 close：忽略，不破坏新连接
        connectedRef.current = false
        connectingRef.current = false
        setConnected(false)
        setConnecting(false)
        closeOpen('assistant')
        closeOpen('thinking')
        if (!intentionalClose.current) {
          setError(tRef.current('session.disconnected'))
          // 自动重连（指数退避：1s → 2s → 4s → … → 最大 30s）。
          const delay = Math.min(1000 * Math.pow(2, reconnectAttemptsRef.current), 30_000)
          reconnectAttemptsRef.current++
          setTimeout(() => {
            // 仅当仍未主动关闭且未建立新连接时才重连
            if (!intentionalClose.current && !connectedRef.current && !connectingRef.current) {
              void connect(sessionIdRef.current)
            }
          }, delay)
        }
      }
      ws.onerror = () => {
        if (!mine()) return
        setError(tRef.current('session.ws_error'))
      }
      ws.onmessage = (e) => {
        // 每次收到服务端帧都刷新活动时间戳（心跳看门狗用）。
        lastActivityRef.current = Date.now()
        let raw: unknown
        try {
          raw = JSON.parse(typeof e.data === 'string' ? e.data : '')
        } catch {
          return
        }
        const frame = parseFrame(raw)
        if (!frame) return

        if (frame.type !== 'text_delta') closeOpen('assistant')
        if (frame.type !== 'thinking_delta') closeOpen('thinking')

        switch (frame.type) {
          case 'state_changed':
            setState(frame.state)
            break
          case 'text_delta':
            appendDelta('assistant', frame.delta)
            break
          case 'thinking_delta':
            appendDelta('thinking', frame.delta)
            break
          case 'say':
            pushItem({ kind: 'say', text: frame.text, level: frame.kind ?? 'info' })
            break
          case 'ask':
            // 去重：seenAskIds 确保同一 ask.id 不会被重复添加（防止服务端偶发重复推送或 StrictMode 导致的双重渲染）。
            if (!seenAskIds.current.has(frame.ask.id)) {
              seenAskIds.current.add(frame.ask.id)
              pushItem({ kind: 'ask', ask: frame.ask })
            }
            break
          case 'tool_exec':
            pushItem({ kind: 'tool', name: frame.name, output: frame.output })
            break
          case 'usage':
            setUsage((u) => addUsage(u, frame.usage))
            break
          case 'usage_snapshot':
            // 用量快照（SET 语义）：连接建立时回放活跃分支累计全量，整体覆盖。
            // 恢复切换会话后的历史用量；与增量 usage（累加）区分，避免重连 / 切换模式时重复累加。
            setUsage(frame.usage)
            break
          case 'done':
            pushItem({
              kind: 'done',
              turns: frame.turns,
              tool_calls: frame.tool_calls,
              success: frame.success,
            })
            setState('idle')
            break
          case 'error':
            pushItem({ kind: 'error', message: frame.message })
            break
          case 'sub_agents':
            setAgents(frame.agents)
            break
          case 'context_usage':
            setContextUsage({ current: frame.current, limit: frame.limit })
            break
        }
      }
    } catch (err) {
      connectingRef.current = false
      setConnecting(false)
      setError(err instanceof Error ? err.message : String(err))
    }
    // guard 已改用 ref，deps 仅保留实际用到的稳定回调；connect 不再随 connecting/connected 重建。
  }, [closeOpen, appendDelta, pushItem, refreshSessions])

  const sendFrame = useCallback((frame: ClientFrame) => {
    if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify(frame))
    }
  }, [])

  const send = useCallback(
    (text: string) => {
      const trimmed = text.trim()
      if (!trimmed || !connected) return
      closeOpen('assistant')
      closeOpen('thinking')
      setItems((prev) => [
        ...prev,
        { id: uid(), kind: 'user', text: trimmed, ts: Date.now() },
      ])
      setStopping(false)
      setState('running')
      sendFrame({ type: 'new_task', text: trimmed, mode: settings.mode })
    },
    [connected, settings.mode, sendFrame, closeOpen],
  )

  const respond = useCallback(
    (askId: string, response: AskResponseValue) => {
      sendFrame({ type: 'respond', ask_id: askId, response })
      setItems((prev) =>
        prev.map((it) =>
          it.kind === 'ask' && it.ask.id === askId
            ? ({
                ...it,
                resolved: typeof response === 'string' ? response : 'text',
                answer: typeof response === 'string' ? undefined : response.text,
              } as TranscriptItem)
            : it,
        ),
      )
    },
    [sendFrame],
  )

  const cancel = useCallback(() => {
    // 即时反馈：进入「停止中」过渡态并立即结束流式气泡（输入光标停止闪烁），再下发 cancel 帧。
    // 后端确认取消后会回传 done/error → state 翻为非 running → 上方 effect 自动复位 stopping。
    // 这样即便存在网络延迟，UI 也能立刻响应「已停止」。
    setStopping(true)
    closeOpen('assistant')
    closeOpen('thinking')
    sendFrame({ type: 'cancel' })
  }, [sendFrame, closeOpen])

  const clear = useCallback(() => {
    setItems([])
    setAgents([])
    setUsage(EMPTY_USAGE)
    setState('no_task')
    openRef.current = {}
    seenAskIds.current = new Set()
  }, [])

  const say = useCallback(
    (text: string, level = 'info') => {
      pushItem({ kind: 'say', text, level })
    },
    [pushItem],
  )

  const newChat = useCallback(() => {
    disconnect()
    clear()
    // reconnect on next tick so state flushes
    setTimeout(() => void connect(null), 50)
  }, [disconnect, clear, connect])

  /** 恢复到指定历史会话：断开 → 拉历史填充 → 重连 resume=id。
   *
   * 后端对「纯 resume 的活跃会话」直接复用（不重建），故切回一个正在运行的会话时
   * 其任务不会被中断。这里先 await 历史拉取再重连，确保「历史覆盖」与「实时帧」
   * 不会竞争——否则 loadHistory 迟到的 setItems 会把刚收到的实时增量回退掉。 */
  const switchSession = useCallback(
    async (id: string) => {
      disconnect()
      clear()
      await loadHistory(id)
      void connect(id)
    },
    [disconnect, clear, loadHistory, connect],
  )

  /** 删除指定历史会话：DELETE 后乐观移除；若删除的是当前会话则新建。 */
  const deleteSession = useCallback(
    async (id: string): Promise<SessionOpResult> => {
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const qs = new URLSearchParams()
      if (cfg.token) qs.set('token', cfg.token)
      try {
        const r = await fetch(`${origin}/api/sessions/${encodeURIComponent(id)}?${qs}`, {
          method: 'DELETE',
        })
        if (!r.ok) {
          const txt = await r.text().catch(() => '')
          throw new Error(txt || `HTTP ${r.status}`)
        }
      } catch (e) {
        return { ok: false, error: e instanceof Error ? e.message : String(e) }
      }
      // 成功：乐观从列表移除（局部动态更新，无需刷新）。
      setSessions((prev) => prev.filter((s) => s.id !== id))
      // 删除的是当前活跃会话：另起新会话，避免停留在已删除的对话上。
      if (id === sessionIdRef.current) newChat()
      return { ok: true }
    },
    [newChat],
  )

  /** 重命名指定历史会话：POST 设置自定义标题后乐观更新列表。 */
  const renameSession = useCallback(
    async (id: string, title: string): Promise<SessionOpResult> => {
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const qs = new URLSearchParams()
      if (cfg.token) qs.set('token', cfg.token)
      const trimmed = title.trim()
      try {
        const r = await fetch(`${origin}/api/sessions/${encodeURIComponent(id)}?${qs}`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ title: trimmed }),
        })
        if (!r.ok) {
          const txt = await r.text().catch(() => '')
          throw new Error(txt || `HTTP ${r.status}`)
        }
      } catch (e) {
        return { ok: false, error: e instanceof Error ? e.message : String(e) }
      }
      setSessions((prev) =>
        prev.map((s) => (s.id === id ? { ...s, title: trimmed } : s)),
      )
      return { ok: true }
    },
    [],
  )

  /** 删除指定对话项（仅 user / assistant）。落盘到服务端日志并就地刷新本地 transcript。
   *
   *  - 历史恢复的项已有 `line`（日志索引），直接删除。
   *  - 本会话实时新增的项暂无 `line`：先拉取历史定位其索引再删除。
   *  - 删除 assistant 时连带移除其同索引的 thinking 块；后续项的 line 按服务端返回的
   *    removed 计数整体下移，保持与日志索引一致（无需整表重拉，保留 say/tool 等本地项）。 */
  const deleteMessage = useCallback(
    async (item: TranscriptItem): Promise<SessionOpResult> => {
      if (item.kind !== 'user' && item.kind !== 'assistant') {
        return { ok: false, error: '仅支持删除用户输入或助手响应' }
      }
      const sid = sessionIdRef.current
      if (!sid) return { ok: false, error: '无活跃会话' }
      // 解析日志行索引：已有则直接用，否则拉历史按顺序定位。
      let line = item.line
      if (line == null) {
        const hist = await fetchHistoryList(sid)
        line = resolveLineIndex(itemsRef.current, item.id, hist)
      }
      if (line == null) {
        return { ok: false, error: '无法定位消息（可能仍在生成中）' }
      }
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const qs = new URLSearchParams()
      if (cfg.token) qs.set('token', cfg.token)
      let removed = 1
      try {
        const res = await fetch(
          `${origin}/api/sessions/${encodeURIComponent(sid)}/messages/${line}?${qs}`,
          { method: 'DELETE' },
        )
        if (res.status === 409) {
          return { ok: false, error: '任务运行中，无法删除' }
        }
        if (!res.ok) {
          const txt = await res.text().catch(() => '')
          throw new Error(txt || `HTTP ${res.status}`)
        }
        const data = await res.json()
        removed = typeof data.removed === 'number' ? data.removed : 1
      } catch (e) {
        return { ok: false, error: e instanceof Error ? e.message : String(e) }
      }
      // 本地就地更新：移除目标项（+ assistant 的同索引 thinking），后续 line 下移。
      const deletedLine = line
      setItems((prev) => {
        const idx = prev.findIndex((it) => it.id === item.id)
        if (idx === -1) return prev
        // 删除区间：目标项 +（assistant 时）前邻同索引 thinking + 后续连续 tool 块，
        // 避免删除一条响应后留下悬空的推理/工具调用块。
        let start = idx
        let end = idx + 1
        if (item.kind === 'assistant') {
          // 先绑定局部变量再判别，便于 TS 正确收窄到 thinking 变体访问 line。
          const before = start > 0 ? prev[start - 1] : null
          if (before && before.kind === 'thinking' && before.line === deletedLine) {
            start -= 1
          }
          while (end < prev.length && prev[end].kind === 'tool') end++
        }
        const spliced = [...prev.slice(0, start), ...prev.slice(end)]
        // 后续项的日志行索引按服务端 removed 计数整体下移，保持与日志一致。
        return spliced.map((it) =>
          'line' in it && it.line != null && it.line > deletedLine
            ? ({ ...it, line: it.line - removed } as TranscriptItem)
            : it,
        )
      })
      return { ok: true }
    },
    [fetchHistoryList],
  )

  /** Switch the active model by starting a fresh session with the given alias. */
  const switchModel = useCallback(
    (alias: string | null) => {
      update({ model: alias })
      clear()
      disconnect()
      setTimeout(() => void connect(null), 60)
    },
    [update, clear, disconnect, connect],
  )

  /** 切换模式：以 resume=当前会话 重连，应用新 system prompt 但保留对话历史。 */
  const switchMode = useCallback(
    (mode: Mode) => {
      if (mode === settings.mode) return
      const keep = sessionId
      update({ mode })
      disconnect()
      setTimeout(() => void connect(keep), 60)
    },
    [settings.mode, sessionId, update, disconnect, connect],
  )

  /** 上传单张图片（base64）到服务端，返回 upload_id 句柄。
   *
   * 把大体积 base64 卸载到 HTTP 信道，避免在 WS 控制信道里传输（受 max_message_size 限制）。
   * 上传失败返回 null（调用方可回退内联发送，依赖 WS 已提升的上限）。 */
  const uploadImage = useCallback(async (mime: string, data: string): Promise<string | null> => {
    const sid = sessionIdRef.current
    if (!sid) return null
    const cfg = settingsRef.current
    const origin = cfg.serverUrl.replace(/\/$/, '')
    const qs = new URLSearchParams()
    if (cfg.token) qs.set('token', cfg.token)
    try {
      const res = await fetch(
        `${origin}/api/sessions/${encodeURIComponent(sid)}/upload?${qs}`,
        {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ mime, data }),
        },
      )
      if (!res.ok) return null
      const d = (await res.json()) as { upload_id?: string }
      return d.upload_id ?? null
    } catch {
      return null
    }
  }, [])

  /** 发送带多模态内容块（图片等）的消息。
   *
   * 内联 image 块先经 /upload 上传拿句柄（image_ref），再经 WS 发送，
   * 避免大 base64 触发 WS 入站消息上限；上传失败则回退内联（依赖 WS 16MiB 上限）。 */
  const sendContent = useCallback(
    (text: string, content: ContentInput[]) => {
      if (!connected) return
      const hasContent = content.length > 0
      const trimmed = text.trim()
      if (!hasContent && !trimmed) return
      closeOpen('assistant')
      closeOpen('thinking')
      setItems((prev) => [
        ...prev,
        { id: uid(), kind: 'user', text: trimmed || '（图片）', ts: Date.now() },
      ])
      setStopping(false)
      setState('running')
      // 异步把内联图片上传为句柄后再发送（不阻塞 UI：用户气泡已即时显示）。
      void (async () => {
        const resolved: ContentInput[] = []
        for (const c of content) {
          if (c.type === 'image' && c.data && c.mime) {
            const uploadId = await uploadImage(c.mime, c.data)
            resolved.push(uploadId ? { type: 'image_ref', upload_id: uploadId } : c)
          } else {
            resolved.push(c)
          }
        }
        sendFrame({
          type: 'new_task',
          text: trimmed,
          mode: settings.mode,
          content: resolved.length > 0 ? resolved : undefined,
        })
      })()
    },
    [connected, settings.mode, sendFrame, closeOpen, uploadImage],
  )

  /** 手动压缩上下文（对应 CLI `/compact`）。 */
  const compact = useCallback(() => {
    sendFrame({ type: 'compact' })
  }, [sendFrame])

  /** 复制当前会话为新 id 后继续（对应 CLI `--fork`）。 */
  const forkSession = useCallback(() => {
    const src = sessionId
    if (!src) return
    clear()
    disconnect()
    setTimeout(() => void connect(null, src), 60)
  }, [sessionId, clear, disconnect, connect])

  /** REST 数据拉取辅助（自定义命令 / skill / mcp / 协同房间）。 */
  const apiGet = useCallback(async <T,>(path: string): Promise<T | null> => {
    const cfg = settingsRef.current
    const origin = cfg.serverUrl.replace(/\/$/, '')
    const qs = new URLSearchParams()
    if (cfg.token) qs.set('token', cfg.token)
    try {
      const res = await fetch(`${origin}${path}${qs.toString() ? `?${qs}` : ''}`)
      if (!res.ok) return null
      return (await res.json()) as T
    } catch {
      return null
    }
  }, [])

  const fetchCustomCommands = useCallback(async () => {
    const d = await apiGet<{ commands: CustomCommandInfo[] }>('/api/commands')
    return d?.commands ?? []
  }, [apiGet])

  const fetchSkills = useCallback(async () => {
    if (!sessionId) return []
    const d = await apiGet<{ skills: SkillInfo[] }>(
      `/api/sessions/${encodeURIComponent(sessionId)}/skills`,
    )
    return d?.skills ?? []
  }, [apiGet, sessionId])

  const fetchSkillBody = useCallback(
    async (name: string) => {
      if (!sessionId) return null
      const d = await apiGet<{ body: string }>(
        `/api/sessions/${encodeURIComponent(sessionId)}/skill/${encodeURIComponent(name)}`,
      )
      return d?.body ?? null
    },
    [apiGet, sessionId],
  )

  const fetchMcp = useCallback(async () => {
    if (!sessionId) return []
    const d = await apiGet<{ tools: McpToolInfo[] }>(
      `/api/sessions/${encodeURIComponent(sessionId)}/mcp`,
    )
    return d?.tools ?? []
  }, [apiGet, sessionId])

  /** 拉取指定会话的分支树（活跃与非活跃会话皆可）。 */
  const fetchBranches = useCallback(
    async (sid: string): Promise<BranchTree | null> => {
      if (!sid) return null
      return apiGet<BranchTree>(`/api/sessions/${encodeURIComponent(sid)}/branches`)
    },
    [apiGet],
  )

  /** 切换活跃分支：POST 后若为当前会话则重连 resume 重载 transcript。 */
  const switchBranch = useCallback(
    async (leafId: string, handoff = false): Promise<SessionOpResult> => {
      const cfg = settingsRef.current
      const origin = cfg.serverUrl.replace(/\/$/, '')
      const sid = sessionIdRef.current
      if (!sid) return { ok: false, error: '无活跃会话' }
      try {
        const res = await fetch(
          `${origin}/api/sessions/${encodeURIComponent(sid)}/branches/switch`,
          {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ leaf_id: leafId, handoff, token: cfg.token ?? null }),
          },
        )
        if (!res.ok) {
          const text = await res.text().catch(() => '')
          return { ok: false, error: text || `HTTP ${res.status}` }
        }
        // 切换成功：重连 resume=当前会话 以重载新分支的 transcript。
        switchSession(sid)
        return { ok: true }
      } catch (e) {
        return { ok: false, error: String(e) }
      }
    },
    [switchSession],
  )

  const newCollabRoom = useCallback(async () => {
    const d = await apiGet<CollabRoom>('/api/collab/room')
    return d ?? null
  }, [apiGet])

  const currentModel = useMemo(
    () => models.find((m) => m.alias === settings.model) ?? models[0] ?? null,
    [models, settings.model],
  )

  // Auto-connect on mount (and when server identity changes while disconnected).
  useEffect(() => {
    if (!connected && !connecting && !sessionId) {
      void connect()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => () => disconnect(), [disconnect])

  const value = useMemo<AgentSessionValue>(
    () => ({
      items,
      state,
      usage,
      contextUsage,
      running,
      stopping,
      connected,
      connecting,
      error,
      sessionId,
      models,
      currentModel,
      stats,
      agents,
      sessions,
      sessionsLoading,
      sessionsError,
      connect,
      disconnect,
      newChat,
      switchSession,
      refreshSessions,
      deleteSession,
      renameSession,
      deleteMessage,
      switchModel,
      switchMode,
      send,
      sendContent,
      compact,
      forkSession,
      respond,
      cancel,
      clear,
      say,
      fetchCustomCommands,
      fetchSkills,
      fetchSkillBody,
      fetchMcp,
      fetchBranches,
      switchBranch,
      newCollabRoom,
    }),
    [
      items,
      state,
      usage,
      contextUsage,
      running,
      stopping,
      connected,
      connecting,
      error,
      sessionId,
      models,
      currentModel,
      stats,
      agents,
      sessions,
      sessionsLoading,
      sessionsError,
      connect,
      disconnect,
      newChat,
      switchSession,
      refreshSessions,
      deleteSession,
      renameSession,
      deleteMessage,
      switchModel,
      switchMode,
      send,
      sendContent,
      compact,
      forkSession,
      respond,
      cancel,
      clear,
      say,
      fetchCustomCommands,
      fetchSkills,
      fetchSkillBody,
      fetchMcp,
      fetchBranches,
      switchBranch,
      newCollabRoom,
    ],
  )

  return (
    <AgentSessionContext.Provider value={value}>{children}</AgentSessionContext.Provider>
  )
}

export function useAgentSession(): AgentSessionValue {
  const ctx = useContext(AgentSessionContext)
  if (!ctx) throw new Error('useAgentSession must be used within AgentSessionProvider')
  return ctx
}
