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
 * free-form text; slash commands are intercepted client-side and mapped to
 * context actions (or skill/command bodies sent as tasks).
 *
 * 所有用户可见文案必须经 `ctx.t()`（locales `cmd.*` 键）——desc 存原文兜底，
 * 内置命令同时给 descKey 供菜单本地化渲染。
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
  /** 本地化函数：命令输出一律走它，禁止硬编码文案。 */
  t: (key: string, args?: Record<string, unknown>) => string
}

export interface Command {
  name: string
  /** 兜底原文（服务端自定义命令的描述，语言不可控）。 */
  desc: string
  /** 内置命令的 locales 键，菜单渲染优先于 desc。 */
  descKey?: string
  /** descKey 的插值参数。 */
  descArgs?: Record<string, unknown>
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
    desc: 'Show available commands',
    descKey: 'cmd.help.desc',
    run: (c) => c.say(c.t('cmd.help.body'), 'info'),
  },
  {
    name: 'status',
    desc: 'Show session status and usage',
    descKey: 'cmd.status.desc',
    run: (c) => c.say(renderStatus(c), 'info'),
  },
  {
    name: 'clear',
    desc: 'Clear the conversation',
    descKey: 'cmd.clear.desc',
    run: (c) => {
      c.clear()
      c.say(c.t('cmd.clear.done'), 'success')
    },
  },
  { name: 'new', desc: 'New session', descKey: 'cmd.new.desc', run: (c) => c.newChat() },
  { name: 'cancel', desc: 'Stop the current task', descKey: 'cmd.cancel.desc', run: (c) => c.cancel() },
  { name: 'files', desc: 'Open file browser', descKey: 'cmd.files.desc', run: (c) => c.openWorkspace() },
  { name: 'settings', desc: 'Open settings', descKey: 'cmd.settings.desc', run: (c) => c.openSettings() },
  {
    name: 'compact',
    desc: 'Compact context (shake + summarize + prune)',
    descKey: 'cmd.compact.desc',
    run: (c) => {
      c.compact()
      c.say(c.t('cmd.compact.started'), 'info')
    },
  },
  {
    name: 'fork',
    desc: 'Fork the current session',
    descKey: 'cmd.fork.desc',
    run: (c) => {
      c.forkSession()
      c.say(c.t('cmd.fork.done'), 'success')
    },
  },
  {
    name: 'skills',
    desc: 'List loaded skills',
    descKey: 'cmd.skills.desc',
    run: async (c) => {
      const list = await c.fetchSkills()
      if (!list.length) {
        c.say(c.t('cmd.skills.empty'), 'info')
        return
      }
      c.say(
        c.t('cmd.skills.list', {
          list: list.map((s) => `- ${s.name} [${s.level}] ${s.description}`).join('\n'),
        }),
        'info',
      )
    },
  },
  {
    name: 'skill',
    desc: 'Inject a skill body (/skill <name>)',
    descKey: 'cmd.skill.desc',
    run: async (c, arg) => {
      if (!arg) {
        c.say(c.t('cmd.skill.usage'), 'warning')
        return
      }
      const body = await c.fetchSkillBody(arg)
      if (body == null) {
        c.say(c.t('cmd.skill.unknown', { name: arg }), 'warning')
        return
      }
      c.send(body)
    },
  },
  {
    name: 'mcp',
    desc: 'List loaded MCP tools',
    descKey: 'cmd.mcp.desc',
    run: async (c) => {
      const list = await c.fetchMcp()
      if (!list.length) {
        c.say(c.t('cmd.mcp.empty'), 'info')
        return
      }
      c.say(
        c.t('cmd.mcp.list', { list: list.map((tl) => `- ${tl.name}  ${tl.description}`).join('\n') }),
        'info',
      )
    },
  },
  {
    name: 'collab',
    desc: 'Create an end-to-end encrypted collab room',
    descKey: 'cmd.collab.desc',
    run: async (c) => {
      const r = await c.newCollabRoom()
      if (!r) {
        c.say(c.t('cmd.collab.failed'), 'danger')
        return
      }
      // guest 页在后端 /collab/{room_id}：路径带房间 id，?wt= 决定可写，# 片段携带
      // E2E 房间密钥（不发给服务器）。此前生成的 /#key 链接无人消费，guest 无法加入。
      const share = `${c.serverOrigin}/collab/${r.room_id}?wt=${r.write_token}#${r.key}`
      c.say(c.t('cmd.collab.room', { id: r.room_id, key: r.key, url: share }), 'success')
    },
  },
  {
    name: 'mode',
    desc: 'Switch mode (code / architect / ask / debug)',
    descKey: 'cmd.mode.desc',
    choices: MODES,
    run: (c, arg) => {
      const m = (MODES as string[]).includes(arg) ? (arg as Mode) : c.mode
      c.switchMode(m)
      c.say(c.t('cmd.mode.set', { mode: m }), 'success')
    },
  },
  {
    name: 'model',
    desc: 'Switch model (starts a new conversation)',
    descKey: 'cmd.model.desc',
    choicesFromModels: true,
    run: (c, arg) => {
      const aliases = c.models.map((m) => m.alias)
      if (!arg || !aliases.includes(arg)) {
        c.say(c.t('cmd.model.usage', { aliases: aliases.join(' | ') }), 'warning')
        return
      }
      c.switchModel(arg === aliases[0] ? null : arg)
      c.say(c.t('cmd.model.switched', { name: arg }), 'success')
    },
  },
]

/** 渲染 `/status` 文本快照（模型 / 模式 / 会话 / 上下文窗口 / token 用量 / 花费）。 */
function renderStatus(c: CommandContext): string {
  const lines: string[] = [c.t('cmd.status.title')]
  lines.push(c.t('cmd.status.model', { v: c.currentModel?.id ?? '—' }))
  lines.push(c.t('cmd.status.mode', { v: c.mode }))
  lines.push(c.t('cmd.status.session', { v: c.sessionId ?? '—' }))
  lines.push(c.t('cmd.status.state', { v: String(c.state) }))
  if (c.contextUsage && c.contextUsage.limit > 0) {
    const { current, limit } = c.contextUsage
    const pct = (current / limit) * 100
    lines.push('')
    lines.push(
      c.t('cmd.status.ctx', { cur: formatNumber(current), limit: formatNumber(limit), pct: pct.toFixed(1) }),
    )
    lines.push(renderBar(pct, 24))
  }
  const u = c.usage
  lines.push('')
  lines.push(c.t('cmd.status.usage'))
  lines.push(c.t('cmd.status.input', { v: formatNumber(u.input_tokens) }))
  lines.push(c.t('cmd.status.output', { v: formatNumber(u.output_tokens) }))
  lines.push(c.t('cmd.status.cache_read', { v: formatNumber(u.cache_read_tokens) }))
  lines.push(c.t('cmd.status.cache_write', { v: formatNumber(u.cache_write_tokens) }))
  if (u.cost_usd > 0) {
    lines.push(c.t('cmd.status.cost', { v: u.cost_usd.toFixed(6) }))
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
    ...(cc.description
      ? { desc: cc.description }
      : { desc: '', descKey: 'cmd.custom.desc', descArgs: { name: cc.name } }),
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
