import type { Mode } from '@/lib/settings'
import type {
  AgentStateName,
  CollabRoom,
  ContentInput,
  CustomCommandInfo,
  McpToolInfo,
  ModelInfo,
  SkillInfo,
  Usage,
} from '@/lib/agent/types'
import { formatNumber } from '@/lib/format'

/**
 * Slash-command registry for the composer. The agent server accepts
 * `new_task` / `respond` / `cancel` / `compact`, so these are client-side actions that
 * mutate UI/session state (clear, switch model/mode, open panels, …) or issue REST
 * fetches (skills / mcp / collab). They mirror the CLI REPL commands where it makes sense.
 */
export interface CommandContext {
  clear: () => void
  newChat: () => void
  cancel: () => void
  switchModel: (alias: string | null) => void
  switchMode: (m: Mode) => void
  models: ModelInfo[]
  say: (text: string, level?: string) => void
  openSettings: () => void
  openWorkspace: () => void
  mode: Mode
  /** 发送文本任务（自定义命令 / skill 注入用）。 */
  send: (text: string) => void
  /** 发送带多模态内容块（图片等）的消息。 */
  sendContent: (text: string, content: ContentInput[]) => void
  /** 手动压缩上下文（CLI `/compact`）。 */
  compact: () => void
  /** 复制当前会话为新 id（CLI `--fork`）。 */
  forkSession: () => void
  /** 拉取已加载 Skill 列表（CLI `/skills`）。 */
  fetchSkills: () => Promise<SkillInfo[]>
  /** 拉取指定 Skill 正文（CLI `/skill:<名>`）。 */
  fetchSkillBody: (name: string) => Promise<string | null>
  /** 拉取已加载 MCP 工具列表（CLI `/mcp`）。 */
  fetchMcp: () => Promise<McpToolInfo[]>
  /** 生成端到端加密协同房间（CLI `/collab`）。 */
  newCollabRoom: () => Promise<CollabRoom | null>
  /** 服务端 origin（协同分享链接用）。 */
  serverOrigin: string
  /** 当前模型（/status 展示用）。 */
  currentModel: ModelInfo | null
  /** 当前会话 id（/status 展示用）。 */
  sessionId: string | null
  /** agent 状态（/status 展示用）。 */
  state: AgentStateName | string
  /** 本会话累计 token 用量（/status 展示用）。 */
  usage: Usage
  /** 上下文窗口 token 占比（/status 展示用）。 */
  contextUsage: { current: number; limit: number } | null
}

export interface Command {
  name: string
  desc: string
  /** Fixed argument choices (e.g. modes). */
  choices?: string[]
  /** Whether arguments come from the model list. */
  choicesFromModels?: boolean
  /** Execute the command; `arg` is the chosen argument (empty when none). */
  run: (ctx: CommandContext, arg: string) => void
}

const MODES: Mode[] = ['code', 'architect', 'ask', 'debug']

/** 内置斜杠命令（镜像 CLI REPL 命令）。 */
export const commands: Command[] = [
  {
    name: 'help',
    desc: '显示可用命令',
    run: (c) =>
      c.say(
        '可用命令：/status /clear /new /cancel /mode /model /compact /skills /skill /mcp /collab /fork /files /settings /help — 输入 `/` 触发菜单',
        'info',
      ),
  },
  {
    name: 'status',
    desc: '查看会话状态与用量',
    run: (c) => c.say(renderStatus(c), 'info'),
  },
  { name: 'clear', desc: '清空当前对话', run: (c) => { c.clear(); c.say('对话已清空', 'success') } },
  { name: 'new', desc: '新建会话', run: (c) => c.newChat() },
  { name: 'cancel', desc: '停止当前任务', run: (c) => c.cancel() },
  { name: 'files', desc: '打开文件浏览', run: (c) => c.openWorkspace() },
  { name: 'settings', desc: '打开设置', run: (c) => c.openSettings() },
  {
    name: 'compact',
    desc: '压缩上下文（shake + summarize + prune）',
    run: (c) => {
      c.compact()
      c.say('正在压缩上下文…', 'info')
    },
  },
  {
    name: 'fork',
    desc: '复制当前会话为新会话',
    run: (c) => {
      c.forkSession()
      c.say('已 fork 当前会话', 'success')
    },
  },
  {
    name: 'skills',
    desc: '列出已加载 skill',
    run: async (c) => {
      const list = await c.fetchSkills()
      if (!list.length) {
        c.say('（未加载 skill）', 'info')
        return
      }
      c.say(
        'Skills：\n' + list.map((s) => `- ${s.name} [${s.level}] ${s.description}`).join('\n'),
        'info',
      )
    },
  },
  {
    name: 'skill',
    desc: '注入指定 skill 正文（/skill <名称>）',
    run: async (c, arg) => {
      if (!arg) {
        c.say('用法：/skill <名称>（/skills 查看列表）', 'warning')
        return
      }
      const body = await c.fetchSkillBody(arg)
      if (body == null) {
        c.say(`未知 skill：${arg}`, 'warning')
        return
      }
      c.send(body)
    },
  },
  {
    name: 'mcp',
    desc: '列出已加载 MCP 工具',
    run: async (c) => {
      const list = await c.fetchMcp()
      if (!list.length) {
        c.say('（未加载 MCP server）', 'info')
        return
      }
      c.say('MCP 工具：\n' + list.map((t) => `- ${t.name}  ${t.description}`).join('\n'), 'info')
    },
  },
  {
    name: 'collab',
    desc: '生成端到端加密协同房间',
    run: async (c) => {
      const r = await c.newCollabRoom()
      if (!r) {
        c.say('生成协同房间失败', 'danger')
        return
      }
      c.say(
        `协同房间（AES-256-GCM）\n房间 id：${r.room_id}\n密钥：${r.key}\n分享链接：${c.serverOrigin}/#${r.key}`,
        'success',
      )
    },
  },
  {
    name: 'mode',
    desc: '切换模式（code / architect / ask / debug）',
    choices: MODES,
    run: (c, arg) => {
      const m = (MODES as string[]).includes(arg) ? (arg as Mode) : c.mode
      c.switchMode(m)
      c.say(`模式已设为 ${m}（重连会话以应用）`, 'success')
    },
  },
  {
    name: 'model',
    desc: '切换模型（开始新对话）',
    choicesFromModels: true,
    run: (c, arg) => {
      const aliases = c.models.map((m) => m.alias)
      if (!arg || !aliases.includes(arg)) {
        c.say(`用法：/model <${aliases.join(' | ')}>`, 'warning')
        return
      }
      c.switchModel(arg === aliases[0] ? null : arg)
      c.say(`已切换模型：${arg}`, 'success')
    },
  },
]

/** 渲染 `/status` 文本快照（模型 / 模式 / 会话 / 上下文窗口 / token 用量 / 花费）。 */
function renderStatus(c: CommandContext): string {
  const lines: string[] = ['📊 状态概览']
  lines.push(`模型：${c.currentModel?.id ?? '—'}`)
  lines.push(`模式：${c.mode}`)
  lines.push(`会话：${c.sessionId ?? '—'}`)
  lines.push(`状态：${c.state}`)
  if (c.contextUsage && c.contextUsage.limit > 0) {
    const { current, limit } = c.contextUsage
    const pct = (current / limit) * 100
    lines.push('')
    lines.push(
      `上下文窗口：${formatNumber(current)} / ${formatNumber(limit)}（${pct.toFixed(1)}%）`,
    )
    lines.push(renderBar(pct, 24))
  }
  const u = c.usage
  lines.push('')
  lines.push('Token 用量')
  lines.push(`  输入：${formatNumber(u.input_tokens)}`)
  lines.push(`  输出：${formatNumber(u.output_tokens)}`)
  lines.push(`  缓存读：${formatNumber(u.cache_read_tokens)}`)
  lines.push(`  缓存写：${formatNumber(u.cache_write_tokens)}`)
  if (u.cost_usd > 0) {
    lines.push(`累计花费：$${u.cost_usd.toFixed(6)}`)
  }
  return lines.join('\n')
}

/** 文本进度条（与 CLI `render_bar` 同构）。 */
function renderBar(pct: number, width: number): string {
  const p = Math.min(100, Math.max(0, pct))
  const filled = Math.round((p / 100) * width)
  const bar = '█'.repeat(filled)
  const empty = '░'.repeat(width - filled)
  return `[${bar}${empty}]`
}

/** 把服务端自定义命令（`.agent/commands/*.md`）映射为本地 Command（注入正文为任务）。 */
export function customCommandsToCommands(custom: CustomCommandInfo[]): Command[] {
  return custom.map((cc) => ({
    name: cc.name,
    desc: cc.description || `自定义命令 /${cc.name}`,
    run: (c, arg) => {
      c.send(arg ? `${cc.body}\n\n# 命令参数\n${arg}` : cc.body)
    },
  }))
}

export interface ParsedCommand {
  /** Command name without the leading slash. */
  name: string
  /** Raw argument string after the first space. */
  arg: string
  /** True once a space separates the command token from its argument. */
  hasArg: boolean
}

/** Parse a raw input line like "/mode code" → { name:'mode', arg:'code', hasArg:true }. */
export function parseCommandLine(input: string): ParsedCommand | null {
  if (!input.startsWith('/')) return null
  const body = input.slice(1)
  const sp = body.indexOf(' ')
  if (sp === -1) return { name: body.toLowerCase(), arg: '', hasArg: false }
  return { name: body.slice(0, sp).toLowerCase(), arg: body.slice(sp + 1).trim(), hasArg: true }
}

/** Choices for a given command (modes or model aliases). */
export function choicesFor(cmd: Command | undefined, models: ModelInfo[]): string[] {
  if (!cmd) return []
  if (cmd.choices) return cmd.choices
  if (cmd.choicesFromModels) return models.map((m) => m.alias)
  return []
}
