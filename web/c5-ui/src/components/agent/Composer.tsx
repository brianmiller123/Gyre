import { useEffect, useMemo, useRef, useState } from 'react'
import { Button, IconButton, Select } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { ModelSwitcher } from '@/components/agent/ModelSwitcher'
import { ApprovalModeSwitcher } from '@/components/agent/ApprovalModeSwitcher'
import { useNotifications } from '@/lib/notifications'
import { useSettings } from '@/lib/settings'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'
import type { ContentInput, CustomCommandInfo } from '@/lib/agent/types'
import {
  choicesFor,
  commands,
  customCommandsToCommands,
  parseCommandLine,
  type Command,
  type CommandContext,
} from '@/lib/agent/commands'
import { expandMentions, parseMentions } from '@/lib/agent/mentions'

const MODE_OPTIONS = [
  { value: 'code', label: 'Code', icon: 'cpu' },
  { value: 'architect', label: 'Architect', icon: 'layers' },
  { value: 'ask', label: 'Ask', icon: 'info' },
  { value: 'debug', label: 'Debug', icon: 'activity' },
] as const

/** 支持的图片 MIME（与 CLI /paste 一致）。 */
const IMAGE_MIMES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']

interface ComposerProps {
  onOpenSettings: () => void
  onOpenWorkspace: () => void
}

/** Message composer: auto-growing textarea, slash-command menu, image upload, mode + send. */
export function Composer({ onOpenSettings, onOpenWorkspace }: ComposerProps) {
  const {
    send,
    sendContent,
    cancel,
    compact,
    forkSession,
    running,
    stopping,
    connected,
    sessionId,
    clear,
    newChat,
    switchModel,
    switchMode,
    models,
    currentModel,
    state,
    usage,
    contextUsage,
    say,
    fetchSkills,
    fetchSkillBody,
    fetchMcp,
    newCollabRoom,
    fetchCustomCommands,
    enhancePrompt,
    apiGet,
  } = useAgentSession()
  const { settings, update } = useSettings()
  const { toast } = useNotifications()
  const { t } = useI18n()
  const [text, setText] = useState('')
  const [active, setActive] = useState(0)
  const [dismissed, setDismissed] = useState(false)
  // 自定义命令（连接后拉取，与内置命令合并进斜杠菜单）。
  const [customCmds, setCustomCmds] = useState<CustomCommandInfo[]>([])
  // 待发送的图片内容块（多模态）。
  const [images, setImages] = useState<ContentInput[]>([])
  const taRef = useRef<HTMLTextAreaElement>(null)
  const listRef = useRef<HTMLUListElement>(null)
  // submit 在途互斥：expandMentions await 期间二次 Enter 会双发同一消息。
  const sendingRef = useRef(false)
  const fileRef = useRef<HTMLInputElement>(null)

  const serverOrigin = useMemo(() => {
    try {
      return settings.serverUrl.replace(/\/$/, '') || window.location.origin
    } catch {
      return ''
    }
  }, [settings.serverUrl])

  const resize = () => {
    const el = taRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`
  }
  useEffect(resize, [text])

  // 连接后拉取自定义命令（与 CLI `.agent/commands/*.md` 同源）。
  useEffect(() => {
    if (connected) void fetchCustomCommands().then(setCustomCmds).catch(() => {})
  }, [connected, fetchCustomCommands])

  // 内置 + 自定义命令合并（斜杠菜单与提交分发共用）。
  const allCommands = useMemo(
    () => [...commands, ...customCommandsToCommands(customCmds)],
    [customCmds],
  )

  // --- slash-command menu state ---
  const parsed = useMemo(() => parseCommandLine(text), [text])
  const isCommand = text.startsWith('/')
  const menuOpen = isCommand && !dismissed

  const cmdExact = parsed ? allCommands.find((c) => c.name === parsed.name) : undefined

  // Phase 1: pick a command (no space yet) → filter by typed name.
  // Phase 2: command chosen, picking an argument → filter choices.
  const list = useMemo<{ label: string; desc: string }[]>(() => {
    if (!menuOpen || !parsed) return []
    if (!parsed.hasArg) {
      return allCommands
        .filter((c) => c.name.startsWith(parsed.name))
        .map((c) => ({ label: c.name, desc: t(c.descKey ?? c.desc, c.descArgs) }))
    }
    if (cmdExact) {
      return choicesFor(cmdExact, models)
        .filter((a) => a.toLowerCase().includes(parsed.arg.toLowerCase()))
        .map((a) => ({ label: a, desc: a === models[0]?.alias ? t('composer.default_model') : '' }))
    }
    return []
  }, [menuOpen, parsed, cmdExact, models, allCommands, t])

  useEffect(() => {
    setActive(0)
  }, [text])

  // 键盘高亮项跟随滚动：命令较多时方向键选中项可能滚出 max-h-64 可视区。
  useEffect(() => {
    const el = listRef.current?.children[active]
    ;(el as HTMLElement | undefined)?.scrollIntoView({ block: 'nearest' })
  }, [active, list])


  const ctx: CommandContext = useMemo(
    () => ({
      clear,
      newChat,
      cancel,
      switchModel,
      switchMode,
      models,
      currentModel,
      sessionId,
      state,
      usage,
      contextUsage,
      say,
      openSettings: onOpenSettings,
      openWorkspace: onOpenWorkspace,
      mode: settings.mode,
      send,
      sendContent,
      compact,
      forkSession,
      fetchSkills,
      fetchSkillBody,
      fetchMcp,
      newCollabRoom,
      serverOrigin,
      t,
    }),
    [
      clear,
      newChat,
      cancel,
      switchModel,
      switchMode,
      update,
      models,
      currentModel,
      sessionId,
      state,
      usage,
      contextUsage,
      say,
      onOpenSettings,
      onOpenWorkspace,
      settings.mode,
      send,
      sendContent,
      compact,
      forkSession,
      fetchSkills,
      fetchSkillBody,
      fetchMcp,
      newCollabRoom,
      serverOrigin,
      t,
    ],
  )

  function runCommand(cmd: Command, arg: string) {
    cmd.run(ctx, arg)
    setText('')
    setDismissed(false)
  }

  async function submit() {
    if (!connected) return
    if (sendingRef.current) return
    const raw = text.trim()
    if (!raw && images.length === 0) return
    sendingRef.current = true
    try {
      if (isCommand) {
        if (running) {
          // 斜杠命令运行中不响应（纯文本作 steering 插话）；必须给出反馈，不能静默。
          toast({ title: t('composer.slash_running'), severity: 'warning' })
          return
        }
        // Execute a typed command directly (e.g. "/clear" or "/mode code").
        const p = parseCommandLine(raw)!
        const cmd = allCommands.find((c) => c.name === p.name)
        if (!cmd) {
          say(t('composer.unknown_cmd', { name: p.name }), 'warning')
          setText('')
          return
        }
        runCommand(cmd, p.arg)
        return
      }
      // 非命令路径：若含 @file 提及，发送前展开为附加上下文块。
      let body = raw
      if (parseMentions(raw).length > 0) {
        body = await expandMentions(raw, { apiGet, say, t })
      }
      // 多模态：有图片时走 sendContent（已展开的文本作为 caption）。
      if (images.length > 0) {
        sendContent(body, images)
        setImages([])
      } else {
        send(body)
      }
      setText('')
    } finally {
      sendingRef.current = false
    }
  }


  function pickActive() {
    if (!menuOpen || list.length === 0) {
      submit()
      return
    }
    const sel = list[active]
    if (!parsed!.hasArg) {
      // Phase 1: a command is highlighted.
      const cmd = allCommands.find((c) => c.name === sel.label)!
      if (cmd.choices || cmd.choicesFromModels) {
        // Move to argument phase.
        setText(`/${cmd.name} `)
      } else {
        runCommand(cmd, '')
      }
    } else if (cmdExact) {
      // Phase 2: an argument is highlighted.
      runCommand(cmdExact, sel.label)
    }
  }

  // ── 图片处理（上传按钮 + 剪贴板粘贴）──
  // 与 CLI read_image 的 MAX_IMAGE_BYTES 对齐：解码后超 10MiB 直接拦截并提示。
  const MAX_IMAGE_BYTES = 10 * 1024 * 1024
  function handleFiles(files: FileList | File[]) {
    const all = Array.from(files)
    const arr = all.filter((f) => IMAGE_MIMES.includes(f.type))
    const rejected = all.length - arr.length
    if (rejected > 0) {
      toast({
        title: t('composer.attach_unsupported', { count: rejected }),
        severity: 'warning',
      })
    }
    for (const f of arr) {
      if (f.size > MAX_IMAGE_BYTES) {
        say(t('composer.image_too_large', { name: f.name || '—' }), 'warning')
        continue
      }
      const reader = new FileReader()
      reader.onload = () => {
        const result = typeof reader.result === 'string' ? reader.result : ''
        // 去掉 `data:<mime>;base64,` 前缀，仅保留 base64 数据。
        const data = result.includes(',') ? result.slice(result.indexOf(',') + 1) : result
        setImages((prev) => [...prev, { type: 'image', mime: f.type, data }])
      }
      reader.readAsDataURL(f)
    }
  }

  const onPaste = (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    // 非图片文件（或混合粘贴）交由 handleFiles 统一拦截并提示，不再静默丢弃。
    const files = Array.from(e.clipboardData.files)
    if (files.length === 0) return
    e.preventDefault()
    handleFiles(files)
  }

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // IME 组合中的 Enter/Escape/方向键属于输入法操作（确认候选词、取消组合、选词），
    // 不应触发发送、停止或菜单导航；keyCode 229 兼容旧浏览器。
    if (e.nativeEvent.isComposing || e.keyCode === 229) return
    if (menuOpen && list.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        setActive((a) => (a + 1) % list.length)
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        setActive((a) => (a - 1 + list.length) % list.length)
        return
      }
      if (e.key === 'Tab') {
        e.preventDefault()
        pickActive()
        return
      }
      if (e.key === 'Escape') {
        e.preventDefault()
        setDismissed(true)
        return
      }
    }
    // 运行中按 Esc 立即停止响应（斜杠菜单打开时 Esc 已在上方用于关闭菜单并 return）。
    if (e.key === 'Escape' && running) {
      e.preventDefault()
      cancel()
      return
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      if (menuOpen && list.length > 0) pickActive()
      else submit()
    }
  }

  return (
    <div className="relative border-t border-border bg-surface/70 backdrop-blur-xl">
      <div className="chat-column py-3">
        {/* Slash-command menu */}
        {menuOpen && list.length > 0 && (
          <div className="absolute bottom-full left-3 right-3 z-30 mb-2 overflow-hidden rounded-xl border border-border bg-surface shadow-pop sm:left-4 sm:right-4">
            <div className="flex items-center gap-1.5 border-b border-border bg-surface-2/70 px-3 py-1.5 text-2xs text-muted">
              <Icon name="command" size={13} className="text-primary" />
              {parsed!.hasArg ? `/${parsed!.name} ${t('composer.slash_args')}` : t('composer.slash_command')}
              <span className="ml-auto flex items-center gap-1">
                <KbdMini>↑↓</KbdMini> {t('composer.hint_select')} <KbdMini>↵</KbdMini> {t('composer.hint_confirm')} <KbdMini>esc</KbdMini> {t('composer.hint_close')}
              </span>
            </div>
            <ul ref={listRef} id="slash-menu" role="listbox" aria-label={t('composer.menu')} className="max-h-64 overflow-y-auto py-1">
              {list.map((item, i) => (
                <li key={item.label} id={`slash-opt-${i}`} role="option" aria-selected={i === active}>
                  <button
                    onMouseEnter={() => setActive(i)}
                    onClick={() => {
                      setActive(i)
                      pickActive()
                    }}
                    className={cn(
                      'flex w-full items-center gap-2.5 px-3 py-2 text-left text-sm',
                      i === active ? 'bg-primary/10 text-text' : 'text-text-2 hover:bg-surface-2',
                    )}
                  >
                    {parsed!.hasArg ? (
                      <Icon name="cube" size={15} className="shrink-0 text-primary" />
                    ) : (
                      <span className="font-mono text-[13px] text-primary">/{item.label}</span>
                    )}
                    {parsed!.hasArg && <span className="font-medium">{item.label}</span>}
                    <span className="ml-auto truncate text-2xs text-muted">{item.desc}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}

        {/* 模型 / 审批模式切换行：输入框上方独立行，两者未就绪前各自隐藏。 */}
        <div className="mb-1.5 flex items-center gap-1.5 px-1">
          <ModelSwitcher />
          <ApprovalModeSwitcher />
        </div>

        {/* 单胶囊输入框：文本区在上，模式/发送收进框内底部工具行。 */}
        <div
          className={cn(
            'rounded-2xl border bg-surface-2 p-2 transition-colors',
            connected
              ? 'border-border focus-within:border-primary/60 focus-within:ring-2 focus-within:ring-primary/15'
              : 'border-border opacity-70',
          )}
        >
          {/* 图片预览条 */}
          {images.length > 0 && (
            <div className="mb-2 flex flex-wrap gap-2">
              {images.map((img, i) => (
                <div key={i} className="group relative">
                  <img
                    src={`data:${img.mime};base64,${img.data}`}
                    alt={t('composer.attachment')}
                    className="h-16 w-16 rounded-lg border border-border object-cover"
                  />
                  <button
                    onClick={() => setImages((prev) => prev.filter((_, j) => j !== i))}
                    className="absolute -right-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full border border-border bg-surface text-muted shadow hover:text-danger"
                    title={t('composer.remove')}
                  >
                    <Icon name="close" size={11} />
                  </button>
                </div>
              ))}
            </div>
          )}

          <textarea
            ref={taRef}
            rows={1}
            role="combobox"
            aria-autocomplete="list"
            aria-label={t('composer.input_aria')}
            aria-expanded={menuOpen && list.length > 0}
            aria-controls="slash-menu"
            aria-activedescendant={menuOpen && list.length > 0 ? `slash-opt-${active}` : undefined}
            value={text}
            onChange={(e) => {
              setText(e.target.value)
              if (dismissed && e.target.value.startsWith('/')) setDismissed(false)
            }}
            onPaste={onPaste}
            onKeyDown={onKeyDown}
            disabled={!connected}
            placeholder={
              connected
                ? running
                  ? t('composer.placeholder.running')
                  : t('composer.placeholder.idle')
                : t('composer.placeholder.connecting')
            }
            className="max-h-[220px] w-full resize-none bg-transparent px-2 py-1.5 text-[14px] text-text placeholder:text-muted/70 focus:outline-none disabled:cursor-not-allowed"
          />

          <div className="mt-1 flex flex-wrap items-center gap-1.5">
            {/* 图片上传 */}
            <input
              ref={fileRef}
              type="file"
              accept={IMAGE_MIMES.join(',')}
              multiple
              className="hidden"
              onChange={(e) => {
                if (e.target.files) handleFiles(e.target.files)
                e.target.value = ''
              }}
            />
            <IconButton
              size="sm"
              icon="image"
              label={t('composer.upload_image')}
              onClick={() => fileRef.current?.click()}
              disabled={!connected}
              className="shrink-0 text-muted hover:bg-surface hover:text-text"
            />
            <EnhanceButton
              text={text}
              setText={setText}
              disabled={!connected}
              enhancePrompt={enhancePrompt}
              say={say}
              t={t}
            />
            <span className="min-w-2 flex-1" />
            <Select
              value={settings.mode}
              onChange={(e) => {
                const m = e.target.value as any
                update({ mode: m })
                switchMode(m)
              }}
              className="h-8 w-auto py-0 text-xs"
              aria-label={t('composer.mode_aria')}
            >
              {MODE_OPTIONS.map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
            </Select>
            {running ? (
              <Button
                size="sm"
                variant="danger"
                leftIcon={stopping ? undefined : 'square'}
                loading={stopping}
                onClick={cancel}
                title={t('composer.stop')}
              >
                {stopping ? t('composer.stopping') : t('composer.stop')}
              </Button>
            ) : (
              <Button
                size="sm"
                variant="primary"
                leftIcon="arrow-right"
                onClick={submit}
                disabled={!connected || (!text.trim() && images.length === 0)}
              >
                {t('composer.send')}
              </Button>
            )}
          </div>
        </div>
        {/* 页脚只保留快捷键提示；连接状态与 session id 由侧栏连接卡唯一承载。 */}
        <div className="mt-1.5 flex items-center justify-end px-1 text-2xs text-muted">
          <span className="flex items-center gap-1">
            <Icon name="command" size={12} /> <span className="font-mono">/</span> {t('composer.footer_cmd')} ·{' '}
            <Icon name="image" size={12} /> {t('composer.footer_paste')} · {t('composer.footer_enter')}
            {running && (
              <>
                {' '}· <KbdMini>esc</KbdMini> {t('composer.footer_stop')}
              </>
            )}
          </span>
        </div>
      </div>
    </div>
  )
}

function KbdMini({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="mx-0.5 inline-flex h-4 min-w-[1rem] items-center justify-center rounded border border-border bg-surface px-1 font-mono text-2xs text-muted">
      {children}
    </kbd>
  )
}

/** ✨ Enhance button (Roo-Code style): single LLM rewrite that replaces the draft. */
function EnhanceButton({
  text,
  setText,
  disabled,
  enhancePrompt,
  say,
  t,
}: {
  text: string
  setText: (v: string) => void
  disabled: boolean
  enhancePrompt: (draft: string) => Promise<string | null>
  say: (msg: string, level?: string) => void
  t: (key: string, args?: Record<string, string | number>) => string
}) {
  const [loading, setLoading] = useState(false)
  const empty = text.trim() === ''

  async function run() {
    if (empty || loading) return
    setLoading(true)
    const out = await enhancePrompt(text.trim())
    setLoading(false)
    if (out !== null && out !== '') setText(out)
    else say(t('composer.enhance_error'), 'warning')
  }

  return (
    <button
      type="button"
      onClick={run}
      disabled={disabled || empty || loading}
      title={t('composer.enhance')}
      aria-label={t('composer.enhance')}
      className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface hover:text-text disabled:cursor-not-allowed"
    >
      <Icon name="sparkles" size={18} className={loading ? 'animate-pulse text-primary' : ''} />
    </button>
  )
}
