import { useEffect, useRef, useState } from 'react'
import { Markdown } from '@/lib/agent/markdown'
import { Icon } from '@/components/icons'
import { Badge, Button } from '@/components/ui'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { askKindLabel, levelMeta, stateMeta } from '@/lib/agent/ui'
import type { AskMessage, TranscriptItem } from '@/lib/agent/types'
import { useNotifications } from '@/lib/notifications'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'

const EXAMPLES = [
  { icon: 'layers', textKey: 'transcript.example.1' },
  { icon: 'cpu', textKey: 'transcript.example.2' },
  { icon: 'search', textKey: 'transcript.example.3' },
  { icon: 'edit', textKey: 'transcript.example.4' },
]

/** Scrollable conversation transcript. */
export function Transcript() {
  const { items, state, connected } = useAgentSession()
  const scrollRef = useRef<HTMLDivElement>(null)
  const endRef = useRef<HTMLDivElement>(null)
  const atBottom = useRef(true)

  const onScroll = () => {
    const el = scrollRef.current
    if (!el) return
    atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 120
  }

  useEffect(() => {
    if (atBottom.current) endRef.current?.scrollIntoView({ behavior: 'smooth', block: 'end' })
  }, [items])

  if (items.length === 0) {
    return <Welcome connected={connected} state={state} />
  }

  return (
    <div ref={scrollRef} onScroll={onScroll} className="no-scrollbar h-full overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl space-y-5 px-4 py-6">
        {items.map((item) => (
          <ItemView key={item.id} item={item} />
        ))}
        <div ref={endRef} className="h-2" />
      </div>
    </div>
  )
}

function Welcome({ connected, state }: { connected: boolean; state: string }) {
  const { send } = useAgentSession()
  const { t } = useI18n()
  const meta = stateMeta[state] ?? stateMeta.no_task
  return (
    <div className="flex h-full items-center justify-center overflow-y-auto p-6">
      <div className="w-full max-w-2xl text-center">
        <div className="mx-auto mb-5 flex h-16 w-16 items-center justify-center rounded-2xl bg-gradient-to-br from-primary to-primary-glow text-white shadow-glow">
          <Icon name="command" size={30} />
        </div>
        <h1 className="font-display text-3xl font-bold tracking-tight text-text">
          <span className="gradient-text">{t('transcript.title_agent')}</span> {t('transcript.title_console')}
        </h1>
        <p className="mx-auto mt-2 max-w-md text-sm text-muted">
          {t('transcript.subtitle')} {connected ? t('transcript.status_connected', { desc: meta.desc }) : t('transcript.connecting')}
        </p>

        <div className="mt-8 grid grid-cols-1 gap-2.5 text-left sm:grid-cols-2">
          {EXAMPLES.map((ex) => (
            <button
              key={ex.textKey}
              disabled={!connected}
              onClick={() => send(t(ex.textKey))}
              className="group flex items-start gap-3 rounded-xl border border-border bg-surface p-3.5 text-left transition-all hover:border-primary/40 hover:shadow-soft disabled:cursor-not-allowed disabled:opacity-50"
            >
              <span className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary transition-colors group-hover:bg-primary/20">
                <Icon name={ex.icon} size={16} />
              </span>
              <span className="text-[13px] leading-relaxed text-text-2">{t(ex.textKey)}</span>
            </button>
          ))}
        </div>

        <p className="mt-6 text-[11px] text-muted">
          提示：在设置中配置 <span className="font-mono">agent --serve</span> 的地址与 token
        </p>
      </div>
    </div>
  )
}

function ItemView({ item }: { item: TranscriptItem }) {
  switch (item.kind) {
    case 'user':
      return <UserMessage item={item} />
    case 'assistant':
      return <AssistantMessage item={item} />
    case 'thinking':
      return <ThinkingBlock text={item.text} streaming={!!item.streaming} />
    case 'tool':
      return <ToolBlock name={item.name} command={item.command} output={item.output} />
    case 'say':
      return <SayLine text={item.text} level={item.level} />
    case 'ask':
      return <AskCard ask={item.ask} resolved={item.resolved} answer={item.answer} />
    case 'error':
      return (
        <div className="flex items-start gap-2.5 rounded-xl border border-danger/30 bg-danger/[0.06] px-3.5 py-2.5 text-sm text-danger">
          <Icon name="x-circle" size={16} className="mt-0.5 shrink-0" />
          <p className="whitespace-pre-wrap break-words">{item.message}</p>
        </div>
      )
    case 'done':
      return (
        <div
          className={cn(
            'flex items-center gap-2.5 rounded-xl border px-3.5 py-2.5 text-sm',
            item.success
              ? 'border-success/30 bg-success/[0.06] text-success'
              : 'border-warning/30 bg-warning/[0.06] text-warning',
          )}
        >
          <Icon name={item.success ? 'check-circle' : 'alert'} size={16} className="shrink-0" />
          <span>
            任务{item.success ? '完成' : '结束'} ·{' '}
            <span className="tabular">{item.turns}</span> 轮 ·{' '}
            <span className="tabular">{item.tool_calls}</span> 次工具调用
          </span>
        </div>
      )
    default:
      return null
  }
}

function Avatar() {
  return (
    <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gradient-to-br from-primary to-primary-glow text-white shadow-sm">
      <Icon name="command" size={16} />
    </span>
  )
}

/**
 * 消息删除按钮：两步确认（首次点击进入确认态 → ✓ 执行 / ✕ 取消）。
 *
 * 仅 user / assistant 项可删除；运行中或流式中禁用。删除失败时弹 toast。
 * 不直接操作 transcript，而是回调会话 hook 的 `deleteMessage`（落盘 + 本地刷新）。
 */
function MessageDeleteButton({
  item,
  confirmKey,
}: {
  item: TranscriptItem
  confirmKey: string
}) {
  const { deleteMessage, running } = useAgentSession()
  const { t } = useI18n()
  const { toast } = useNotifications()
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)

  const streaming = 'streaming' in item && !!item.streaming
  const disabled = running || busy || streaming

  const doDelete = async () => {
    setBusy(true)
    const res = await deleteMessage(item)
    setBusy(false)
    setConfirming(false)
    if (!res.ok) {
      toast({
        title: t('transcript.delete_failed'),
        body: res.error,
        severity: 'danger',
      })
    }
  }

  if (confirming) {
    return (
      <span className="inline-flex items-center gap-1">
        <span className="mr-0.5 text-[11px] text-danger">{t(confirmKey)}</span>
        <button
          type="button"
          onClick={doDelete}
          disabled={busy}
          title={t('common.confirm')}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-[11px] font-medium text-danger transition-colors hover:bg-danger/10 disabled:opacity-50"
        >
          <Icon name="check" size={12} />
        </button>
        <button
          type="button"
          onClick={() => setConfirming(false)}
          disabled={busy}
          title={t('common.cancel')}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-[11px] text-muted transition-colors hover:bg-surface-2 hover:text-text-2 disabled:opacity-50"
        >
          <Icon name="close" size={12} />
        </button>
      </span>
    )
  }

  return (
    <button
      type="button"
      onClick={() => setConfirming(true)}
      disabled={disabled}
      title={disabled ? t('transcript.delete_running') : t('transcript.delete')}
      aria-label={t('transcript.delete')}
      className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] text-muted transition-colors hover:bg-danger/10 hover:text-danger disabled:cursor-not-allowed disabled:opacity-40"
    >
      <Icon name="trash" size={12} />
      {t('common.delete')}
    </button>
  )
}

/**
 * 用户输入气泡：右对齐，悬停（移动端常驻）显示删除按钮。
 *
 * 删除按钮绝对定位于气泡左侧（right-full），不占据布局空间，避免气泡位移。
 */
function UserMessage({ item }: { item: Extract<TranscriptItem, { kind: 'user' }> }) {
  return (
    <div className="group flex justify-end">
      <div className="relative max-w-[85%]">
        <div className="absolute right-full top-0 mr-1 flex items-center opacity-100 sm:opacity-0 sm:transition-opacity sm:group-hover:opacity-100 sm:focus-within:opacity-100">
          <MessageDeleteButton item={item} confirmKey="transcript.delete_confirm_user" />
        </div>
        <div className="rounded-2xl rounded-br-md bg-primary px-4 py-2.5 text-[14px] leading-relaxed text-white dark:text-[#06241f]">
          <p className="whitespace-pre-wrap break-words">{item.text}</p>
        </div>
      </div>
    </div>
  )
}

/**
 * Assistant 回复气泡：Markdown 渲染 + 流式光标 + 一键复制整条响应。
 *
 * 复制写入的是原始 markdown（`item.text`），而非渲染后的纯文本，便于
 * 贴回编辑器或其它会话。流式过程中隐藏按钮（内容仍在增长），结束后
 * 常驻显示——移动端无 hover，故按钮默认可见；桌面端用 group-hover 淡入。
 */
function AssistantMessage({ item }: { item: Extract<TranscriptItem, { kind: 'assistant' }> }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const hasText = item.text.trim().length > 0

  const copy = () => {
    if (!hasText) return
    navigator.clipboard?.writeText(item.text).then(
      () => {
        setCopied(true)
        window.setTimeout(() => setCopied(false), 1500)
      },
      () => {},
    )
  }

  return (
    <div className="group flex gap-3">
      <Avatar />
      <div className="min-w-0 flex-1 pt-0.5">
        {hasText ? (
          <Markdown>{item.text}</Markdown>
        ) : (
          <p className="text-sm text-muted">…</p>
        )}
        {item.streaming && (
          <span className="ml-0.5 inline-block h-4 w-1.5 animate-pulse rounded-sm bg-primary align-middle" />
        )}
        {hasText && !item.streaming && (
          <div className="mt-1.5 flex items-center gap-2 sm:opacity-0 sm:transition-opacity sm:group-hover:opacity-100 sm:focus-within:opacity-100">
            <button
              type="button"
              onClick={copy}
              title={copied ? t('common.copied') : t('common.copy')}
              aria-label={copied ? t('common.copied') : t('common.copy')}
              className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] text-muted transition-colors hover:bg-surface-2 hover:text-text-2"
            >
              <Icon name={copied ? 'check' : 'copy'} size={12} />
              {copied ? t('common.copied') : t('common.copy')}
            </button>
            <MessageDeleteButton item={item} confirmKey="transcript.delete_confirm_assistant" />
          </div>
        )}
      </div>
    </div>
  )
}

function ThinkingBlock({ text, streaming }: { text: string; streaming: boolean }) {
  const [open, setOpen] = useState(true)
  const { t } = useI18n()
  return (
    <div className="rounded-xl border border-border bg-surface-2/50">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-xs font-medium text-muted"
      >
        <Icon name="activity" size={14} className={streaming ? 'animate-pulse text-primary' : ''} />
        {t('transcript.thinking')}
        {streaming && <Badge tone="primary" className="px-1.5 py-0 text-[10px]">{t('transcript.generating')}</Badge>}
        <span className="flex-1" />
        <Icon name="chevron-down" size={14} className={cn('transition-transform', open && 'rotate-180')} />
      </button>
      {open && (
        <div className="max-h-60 overflow-y-auto border-t border-border px-3 py-2.5">
          <p className="whitespace-pre-wrap break-words font-mono text-[12px] italic leading-relaxed text-muted">
            {text}
          </p>
        </div>
      )}
    </div>
  )
}

function ToolBlock({ name, command, output }: { name: string; command?: string; output: string }) {
  const [open, setOpen] = useState(false)
  const hasOutput = output.trim().length > 0
  return (
    <div className="rounded-xl border border-border bg-surface-2/50">
      <button
        onClick={() => (hasOutput || !!command) && setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-3 py-2 text-xs"
      >
        <span className="flex h-6 w-6 items-center justify-center rounded-md bg-primary/10 text-primary">
          <Icon name="cpu" size={13} />
        </span>
        <span className="font-mono font-medium text-text-2">{name}</span>
        {/* 被执行的命令/主操作数：yolo 等无审批帧的模式下也能一眼看到工具在做什么。 */}
        {command && (
          <code
            className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted"
            title={command}
          >
            {command}
          </code>
        )}
        {hasOutput && (
          <Icon
            name="chevron-down"
            size={13}
            className={cn('text-muted transition-transform', open && 'rotate-180', !command && 'ml-auto')}
          />
        )}
      </button>
      {open && hasOutput && (
        <pre className="max-h-72 overflow-auto border-t border-border bg-[#0b0d12] p-3 text-[12px] leading-relaxed text-white/85 dark:bg-[#070809]">
          <code className="font-mono whitespace-pre-wrap break-words">{output}</code>
        </pre>
      )}
    </div>
  )
}

function SayLine({ text, level }: { text: string; level: string }) {
  const meta = levelMeta[level] ?? levelMeta.info
  const toneText: Record<string, string> = {
    info: 'text-info',
    success: 'text-success',
    warning: 'text-warning',
    danger: 'text-danger',
    error: 'text-danger',
    neutral: 'text-muted',
  }
  return (
    <div className="flex items-center gap-2 py-0.5 text-[12px]">
      <Icon name={meta.icon} size={13} className={cn('shrink-0', toneText[level] ?? 'text-muted')} />
      <span className="text-muted">{text}</span>
    </div>
  )
}

function AskCard({
  ask,
  resolved,
  answer,
}: {
  ask: AskMessage
  resolved?: 'yes' | 'no' | 'text'
  answer?: string
}) {
  const { respond } = useAgentSession()
  const { t } = useI18n()
  const [text, setText] = useState('')
  const isFollowup = typeof ask.kind === 'string' && ask.kind === 'followup'

  return (
    <div className="rounded-xl border border-primary/30 bg-primary/[0.04] p-3.5 shadow-soft">
      <div className="mb-2 flex items-center gap-2">
        <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-primary/15 text-primary">
          <Icon name="shield" size={15} />
        </span>
        <Badge tone="info">{askKindLabel(ask.kind)}</Badge>
      </div>
      <p className="mb-3 whitespace-pre-wrap break-words text-[13px] leading-relaxed text-text">{ask.prompt}</p>

      {resolved ? (
        <div className="flex items-center gap-2 text-xs">
          <Icon
            name={resolved === 'yes' ? 'check-circle' : resolved === 'no' ? 'x-circle' : 'check'}
            size={14}
            className={resolved === 'no' ? 'text-danger' : 'text-success'}
          />
          <span className="text-muted">
            {resolved === 'yes' ? t('transcript.approved') : resolved === 'no' ? t('transcript.rejected') : t('transcript.replied', { answer: answer ?? '' })}
          </span>
        </div>
      ) : isFollowup ? (
        <div className="flex items-end gap-2">
          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            rows={1}
            placeholder={t('transcript.reply_placeholder')}
            className="max-h-32 flex-1 resize-none rounded-lg border border-border bg-surface px-3 py-2 text-[13px] text-text placeholder:text-muted/60 focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/15"
          />
          <Button
            variant="primary"
            size="md"
            leftIcon="arrow-right"
            disabled={!text.trim()}
            onClick={() => respond(ask.id, { text: text.trim() })}
          >
            回复
          </Button>
        </div>
      ) : (
        <div className="flex gap-2">
          <Button variant="primary" leftIcon="check" onClick={() => respond(ask.id, 'yes')}>
            批准
          </Button>
          <Button variant="outline" leftIcon="close" className="text-danger hover:bg-danger/10" onClick={() => respond(ask.id, 'no')}>
            拒绝
          </Button>
        </div>
      )}
    </div>
  )
}
