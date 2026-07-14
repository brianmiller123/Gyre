/**
 * Wire-protocol types for the agent server (mirrors `agent_server::ServerFrame`
 * / `ClientFrame` and `agent_core` message types). The server serializes with
 * serde internally-tagged enums (`tag = "type"`); struct variants (and
 * newtype variants wrapping a struct/map) flatten their fields to the top
 * level — `parseFrame` normalizes defensively to absorb serde quirks.
 */
import type { Mode } from '@/lib/settings'

/** Toast / inline message severity. */
export type Severity = 'info' | 'success' | 'warning' | 'danger'

export type AgentStateName =
  | 'no_task'
  | 'running'
  | 'streaming'
  | 'waiting_for_input'
  | 'idle'
  | 'resumable'

export interface Usage {
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  cost_usd: number
}

/** 子 Agent 生命周期阶段（镜像 agent_supervisor::SubAgentPhase）。 */
export type SubAgentPhase =
  | 'pending'
  | 'running'
  | 'streaming'
  | 'waiting_tool'
  | 'done'
  | 'failed'
  | 'cancelled'

/** 子 Agent 日志级别。 */
export type SubAgentLogLevel = 'info' | 'debug' | 'warn' | 'error'

/** 子 Agent 日志行。 */
export interface SubAgentLogLine {
  ts: number
  level: SubAgentLogLevel
  text: string
}

/** 单个子 Agent 的实时状态快照（镜像 agent_supervisor::SubAgentStatus）。 */
export interface SubAgentStatus {
  id: string
  parent_id: string | null
  label: string
  task: string
  phase: SubAgentPhase
  progress: number
  turns: number
  tool_calls: number
  usage: Usage
  started_at: number
  updated_at: number
  current_activity: string | null
  error: string | null
  logs: SubAgentLogLine[]
}

/** AskKind is externally tagged: `{"tool":{"tool":"x"}}` | `{"command":{"command":"..."}}` | `"followup"` | `"completion_result"`. */
export type AskKind =
  | { tool: { tool: string } }
  | { command: { command: string } }
  | 'followup'
  | 'completion_result'

export interface AskMessage {
  id: string
  kind: AskKind
  prompt: string
}

/** Value sent back as `ClientFrame::Respond.response`. */
export type AskResponseValue = 'yes' | 'no' | { text: string }

export type Frame =
  | { type: 'state_changed'; state: AgentStateName | string }
  | { type: 'text_delta'; delta: string }
  | { type: 'thinking_delta'; delta: string }
  | { type: 'say'; text: string; kind?: string }
  | { type: 'ask'; ask: AskMessage }
  | { type: 'tool_exec'; name: string; output: string }
  | { type: 'usage'; usage: Usage }
  | { type: 'done'; turns: number; tool_calls: number; success: boolean }
  | { type: 'error'; message: string }
  | { type: 'sub_agents'; agents: SubAgentStatus[] }
  | { type: 'context_usage'; current: number; limit: number }

/** Normalize a raw JSON message into a typed Frame (tolerant of serde quirks). */
export function parseFrame(raw: unknown): Frame | null {
  if (!raw || typeof raw !== 'object') return null
  const r = raw as Record<string, any>
  const type: string = r.type
  switch (type) {
    case 'state_changed':
      return { type: 'state_changed', state: r.state_changed ?? r.state ?? r.StateChanged ?? 'idle' }
    case 'text_delta':
      return { type: 'text_delta', delta: r.delta ?? '' }
    case 'thinking_delta':
      return { type: 'thinking_delta', delta: r.delta ?? '' }
    case 'say':
      return { type: 'say', text: r.text ?? '', kind: r.kind }
    case 'ask':
      return { type: 'ask', ask: r.ask as AskMessage }
    case 'tool_exec':
      return { type: 'tool_exec', name: r.name ?? '', output: r.output ?? '' }
    case 'usage': {
      const u = r.usage && typeof r.usage === 'object' ? r.usage : r
      return {
        type: 'usage',
        usage: {
          input_tokens: u.input_tokens ?? 0,
          output_tokens: u.output_tokens ?? 0,
          cache_read_tokens: u.cache_read_tokens ?? 0,
          cache_write_tokens: u.cache_write_tokens ?? 0,
          cost_usd: u.cost_usd ?? 0,
        },
      }
    }
    case 'done':
      return {
        type: 'done',
        turns: r.turns ?? 0,
        tool_calls: r.tool_calls ?? r.toolCalls ?? 0,
        success: !!r.success,
      }
    case 'error':
      return { type: 'error', message: r.message ?? '' }
    case 'sub_agents':
      return {
        type: 'sub_agents',
        agents: Array.isArray(r.agents) ? (r.agents as SubAgentStatus[]) : [],
      }
    case 'context_usage':
      return {
        type: 'context_usage',
        current: typeof r.current === 'number' ? r.current : 0,
        limit: typeof r.limit === 'number' ? r.limit : 0,
      }
    default:
      return null
  }
}

/* -------- Multimodal content (image upload / paste) -------- */
export interface ContentInput {
  type: 'text' | 'image' | 'image_ref'
  text?: string
  mime?: string
  data?: string
  upload_id?: string
}

/* ----------------------------- Client → Server ---------------------------- */
export interface NewTaskFrame {
  type: 'new_task'
  text: string
  mode: Mode | null
  content?: ContentInput[]
}
export interface RespondFrame {
  type: 'respond'
  ask_id: string
  response: AskResponseValue
}
export type ClientFrame =
  | NewTaskFrame
  | RespondFrame
  | { type: 'cancel' }
  | { type: 'compact' }

/* ------------------------------- UI models -------------------------------- */
export type TranscriptItem =
  | { id: string; kind: 'user'; text: string; ts: number; line?: number }
  | { id: string; kind: 'assistant'; text: string; ts: number; streaming?: boolean; line?: number }
  | { id: string; kind: 'thinking'; text: string; ts: number; streaming?: boolean; line?: number }
  | { id: string; kind: 'tool'; name: string; output: string; ts: number }
  | { id: string; kind: 'say'; text: string; level: string; ts: number }
  | {
      id: string
      kind: 'ask'
      ask: AskMessage
      ts: number
      resolved?: 'yes' | 'no' | 'text'
      answer?: string
    }
  | { id: string; kind: 'error'; message: string; ts: number }
  | { id: string; kind: 'done'; turns: number; tool_calls: number; success: boolean; ts: number }

export interface ModelInfo {
  alias: string
  id: string
  api: string
}

/** 历史会话列表项（含首条用户输入预览，与 CLI `/sessions` 同源）。 */
export interface SessionListItem {
  id: string
  /** 首条用户输入的预览文本（无自定义标题时的回退展示）。 */
  preview: string
  /** 用户自定义标题（重命名）；为 null/空时回退到 preview。 */
  title?: string | null
  mtime_ms: number
  bytes: number
}

/** 会话操作结果：成功 ok=true；失败附带可展示的 error 文案。 */
export interface SessionOpResult {
  ok: boolean
  error?: string
}

/** 会话历史消息（user/thinking/assistant 文本，恢复对话展示用）。 */
export interface SessionHistoryItem {
  kind: 'user' | 'assistant' | 'thinking'
  text: string
  /** 该消息在服务端日志中的索引（与 DELETE /messages/{line} 同源）。 */
  line: number
}

/** 自定义 slash 命令（`.agent/commands/*.md`，与 CLI 同源）。 */
export interface CustomCommandInfo {
  name: string
  description: string
  body: string
}

/** Skill 列表项（`/api/sessions/{id}/skills`）。 */
export interface SkillInfo {
  name: string
  description: string
  level: 'user' | 'project'
  hide: boolean
}

/** MCP 工具列表项（`/api/sessions/{id}/mcp`）。 */
export interface McpToolInfo {
  name: string
  description: string
}

/** 协同房间（`/api/collab/room`）。 */
export interface CollabRoom {
  room_id: string
  key: string
  ws_url: string
}
