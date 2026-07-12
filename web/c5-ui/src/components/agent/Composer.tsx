import { useEffect, useMemo, useRef, useState } from 'react'
import { Button, Select } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useAgentSession } from '@/lib/agent/useAgentSession'
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
  } = useAgentSession()
  const { settings, update } = useSettings()
  const { t } = useI18n()
  const [text, setText] = useState('')
  const [active, setActive] = useState(0)
  const [dismissed, setDismissed] = useState(false)
  // 自定义命令（连接后拉取，与内置命令合并进斜杠菜单）。
  const [customCmds, setCustomCmds] = useState<CustomCommandInfo[]>([])
  // 待发送的图片内容块（多模态）。
  const [images, setImages] = useState<ContentInput[]>([])
  const taRef = useRef<HTMLTextAreaElement>(null)
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
  const expectArg = !!(cmdExact && (cmdExact.choices || cmdExact.choicesFromModels))

  // Phase 1: pick a command (no space yet) → filter by typed name.
  // Phase 2: command chosen, picking an argument → filter choices.
  const list = useMemo<{ label: string; desc: string }[]>(() => {
    if (!menuOpen || !parsed) return []
    if (!parsed.hasArg) {
      return allCommands
        .filter((c) => c.name.startsWith(parsed.name))
        .map((c) => ({ label: c.name, desc: c.desc }))
    }
    if (cmdExact) {
      return choicesFor(cmdExact, models)
        .filter((a) => a.toLowerCase().includes(parsed.arg.toLowerCase()))
        .map((a) => ({ label: a, desc: a === models[0]?.alias ? t('composer.default_model') : '' }))
    }
    return []
  }, [menuOpen, parsed, cmdExact, models, allCommands])

  useEffect(() => {
    setActive(0)
  }, [text])

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
    ],
  )

  function runCommand(cmd: Command, arg: string) {
    cmd.run(ctx, arg)
    setText('')
    setDismissed(false)
  }

  function submit() {
    if (running || !connected) return
    const t = text.trim()
    if (!t && images.length === 0) return
    if (isCommand) {
      // Execute a typed command directly (e.g. "/clear" or "/mode code").
      const p = parseCommandLine(t)!
      const cmd = allCommands.find((c) => c.name === p.name)
      if (!cmd) {
        say(`未知命令：/${p.name}（输入 /help 查看）`, 'warning')
        setText('')
        return
      }
      runCommand(cmd, p.arg)
      return
    }
    // 多模态：有图片时走 sendContent。
    if (images.length > 0) {
      sendContent(t, images)
      setImages([])
    } else {
      send(t)
    }
    setText('')
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
  function handleFiles(files: FileList | File[]) {
    const arr = Array.from(files).filter((f) => IMAGE_MIMES.includes(f.type))
    for (const f of arr) {
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
    const files = Array.from(e.clipboardData.items)
      .map((it) => it.getAsFile())
      .filter((f): f is File => !!f && IMAGE_MIMES.includes(f.type))
    if (files.length > 0) {
      e.preventDefault()
      handleFiles(files)
    }
  }

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
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
      <div className="mx-auto w-full max-w-3xl px-3 py-3 sm:px-4">
        {/* Slash-command menu */}
        {menuOpen && list.length > 0 && (
          <div className="absolute bottom-full left-3 right-3 z-30 mb-2 overflow-hidden rounded-xl border border-border bg-surface shadow-pop sm:left-4 sm:right-4">
            <div className="flex items-center gap-1.5 border-b border-border bg-surface-2/70 px-3 py-1.5 text-[11px] text-muted">
              <Icon name="command" size={13} className="text-primary" />
              {parsed!.hasArg ? `/${parsed!.name} ${t('composer.slash_args')}` : t('composer.slash_command')}
              <span className="ml-auto flex items-center gap-1">
                <KbdMini>↑↓</KbdMini> {t('composer.hint_select')} <KbdMini>↵</KbdMini> {t('composer.hint_confirm')} <KbdMini>esc</KbdMini> {t('composer.hint_close')}
              </span>
            </div>
            <ul className="max-h-64 overflow-y-auto py-1">
              {list.map((item, i) => (
                <li key={item.label}>
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
                    <span className="ml-auto truncate text-[11px] text-muted">{item.desc}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}

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

          <div className="flex items-end gap-2">
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
            <button
              onClick={() => fileRef.current?.click()}
              disabled={!connected}
              title={t('composer.upload_image')}
              className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface hover:text-text disabled:cursor-not-allowed"
            >
              <Icon name="image" size={18} />
            </button>

            <textarea
              ref={taRef}
              rows={1}
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
              className="max-h-[220px] flex-1 resize-none bg-transparent px-2 py-2 text-[14px] text-text placeholder:text-muted/70 focus:outline-none disabled:cursor-not-allowed"
            />
            <div className="flex items-center gap-2">
              <Select
                value={settings.mode}
                onChange={(e) => {
                  const m = e.target.value as any
                  update({ mode: m })
                  switchMode(m)
                }}
                className="h-9 w-auto py-0 text-xs"
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
        </div>
        <div className="mt-1.5 flex items-center justify-between px-1 text-[11px] text-muted">
          <span className="truncate">
            {connected ? (
              <>
                <span className="inline-block h-1.5 w-1.5 rounded-full bg-success" /> {t('sidebar.connected')} ·{' '}
                <span className="font-mono">{sessionId ? sessionId.slice(0, 8) : '—'}</span>
              </>
            ) : (
              <>
                <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-warning" />{' '}
                {t('sidebar.disconnected')}
              </>
            )}
          </span>
          <span className="hidden items-center gap-1 sm:flex">
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
    <kbd className="mx-0.5 inline-flex h-4 min-w-[1rem] items-center justify-center rounded border border-border bg-surface px-1 font-mono text-[10px] text-muted">
      {children}
    </kbd>
  )
}
