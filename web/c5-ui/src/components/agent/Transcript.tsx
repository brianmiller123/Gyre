import { memo, useEffect, useMemo, useRef, useState } from 'react'
import { Markdown } from '@/lib/agent/markdown'
import { Icon } from '@/components/icons'
import { Badge, Button } from '@/components/ui'
import { useAgentSession, useTranscriptItems } from '@/lib/agent/useAgentSession'
import { askKindLabel, levelMeta, stateMeta } from '@/lib/agent/ui'
import type { AskMessage, TranscriptItem } from '@/lib/agent/types'
import { useNotifications } from '@/lib/notifications'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'
import { copyText } from '@/lib/clipboard'

const EXAMPLES = [
  { icon: 'layers', textKey: 'transcript.example.1' },
  { icon: 'cpu', textKey: 'transcript.example.2' },
  { icon: 'search', textKey: 'transcript.example.3' },
  { icon: 'edit', textKey: 'transcript.example.4' },
]

/** Scrollable conversation transcript. */
export function Transcript() {
  // items 走独立高频 context：仅本组件随流式 flush 重渲染（其余面板订阅主 context 不受影响）。
  const items = useTranscriptItems()
  const { state, connected } = useAgentSession()
  const { t } = useI18n()
  const scrollRef = useRef<HTMLDivElement>(null)
  const endRef = useRef<HTMLDivElement>(null)
  const atBottom = useRef(true)
  const [showJump, setShowJump] = useState(false)

  const onScroll = () => {
    const el = scrollRef.current
    if (!el) return
    const atEnd = el.scrollHeight - el.scrollTop - el.clientHeight < 120
    atBottom.current = atEnd
    setShowJump(!atEnd)
  }

  useEffect(() => {
    if (atBottom.current) endRef.current?.scrollIntoView({ behavior: 'auto', block: 'end' })
  }, [items])

  if (items.length === 0) {
    return <Welcome connected={connected} state={state} />
  }

  return (
    // role="log"（隐含 aria-live=polite）：新消息对读屏用户可感知（WCAG 4.1.3 状态消息）。
    <div className="relative h-full">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        role="log"
        aria-live="polite"
        aria-label={t('transcript.log_aria')}
        className="h-full overflow-y-auto"
      >
        {/* 回合分组：用户消息开启新回合，回合间距大于回合内条目间距。 */}
        <div className="chat-column py-6">
          {items.map((item, i) => {
            const turnStart = item.kind === 'user' && i > 0
            const prev = items[i - 1]
            return (
              <div key={item.id} className={cn(i > 0 && (turnStart ? 'mt-9' : 'mt-5'))}>
                <ItemView
                  item={item}
                  showAvatar={item.kind === 'assistant' && prev?.kind !== 'assistant'}
                />
              </div>
            )
          })}
          <div ref={endRef} className="h-2" />
        </div>
      </div>
      {/* 上翻阅读时提供「回到底部」，避免长会话里手动滚回。
          按钮挂在同一条 chat-column 上：否则视口越宽，它离正文右边界越远。 */}
      {showJump && (
        <div className="pointer-events-none absolute inset-x-0 bottom-4">
          <div className="chat-column relative">
            <button
              type="button"
              onClick={() => endRef.current?.scrollIntoView({ behavior: 'smooth', block: 'end' })}
              aria-label={t('transcript.jump_latest')}
              title={t('transcript.jump_latest')}
              className="animate-fade-in pointer-events-auto absolute bottom-0 right-4 flex h-9 w-9 items-center justify-center rounded-full border border-border bg-surface text-text-2 shadow-pop transition-colors hover:bg-surface-2 hover:text-text"
            >
              <Icon name="chevron-down" size={17} />
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

function Welcome({ connected, state }: { connected: boolean; state: string }) {
  const { send } = useAgentSession()
  const { t } = useI18n()
  const meta = stateMeta[state] ?? stateMeta.no_task
  return (
    <div className="flex h-full items-center justify-center overflow-y-auto py-8">
      <div className="chat-column text-center">
        <div className="mx-auto mb-5 flex h-16 w-16 items-center justify-center rounded-2xl bg-gradient-to-br from-primary to-primary-glow text-white shadow-glow">
          <Icon name="command" size={30} />
        </div>
        <h1 className="font-display text-3xl font-bold tracking-tight text-text">
          <span className="gradient-text">{t('transcript.title_agent')}</span> {t('transcript.title_console')}
        </h1>
        <p className="mx-auto mt-2 max-w-md text-sm text-muted">
          {t('transcript.subtitle')}{' '}
          {connected ? t('transcript.status_connected', { desc: t(meta.desc) }) : t('transcript.connecting')}
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

        <p className="mt-6 text-2xs text-muted">
          {t('transcript.hint_serve')}
        </p>
      </div>
    </div>
  )
}

const ItemView = memo(function ItemView({
  item,
  showAvatar = true,
}: {
  item: TranscriptItem
  /** 连续 assistant 消息只在首条渲染头像，减少长回合里的重复装饰。 */
  showAvatar?: boolean
}) {
  const { t } = useI18n()
  switch (item.kind) {
    case 'user':
      return <UserMessage item={item} />
    case 'assistant':
      return <AssistantMessage item={item} showAvatar={showAvatar} />
    case 'thinking':
      return <ThinkingBlock text={item.text} streaming={!!item.streaming} ts={item.ts} />
    case 'tool':
      return <ToolBlock name={item.name} command={item.command} output={item.output} ts={item.ts} />
    case 'say':
      return <SayLine text={item.text} level={item.level} />
    case 'ask':
      return <AskCard ask={item.ask} resolved={item.resolved} answer={item.answer} />
    case 'error':
      return (
        // role="alert"：错误插入时立即播报，不等用户滚动到该条目。
        <div
          role="alert"
          className="flex items-start gap-2.5 rounded-xl border border-danger/30 bg-danger/[0.06] px-3.5 py-2.5 text-sm text-danger"
        >
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
            {t(item.success ? 'transcript.done_ok' : 'transcript.done_end')} ·{' '}
            <span className="tabular">{item.turns}</span> {t('transcript.turns')} ·{' '}
            <span className="tabular">{item.tool_calls}</span> {t('transcript.tool_calls')}
          </span>
        </div>
      )
    default:
      return null
  }
})

function Avatar() {
  return (
    <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gradient-to-br from-primary to-primary-glow text-white shadow-sm">
      <Icon name="command" size={16} />
    </span>
  )
}

/** 消息时间（HH:mm），随界面语言本地化；ts 缺失时返回空串。 */
function fmtTime(ts: number | undefined, locale: string): string {
  if (!ts) return ''
  try {
    return new Date(ts).toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit', hour12: false })
  } catch {
    return ''
  }
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
        <span className="mr-0.5 text-2xs text-danger">{t(confirmKey)}</span>
        <button
          type="button"
          onClick={doDelete}
          disabled={busy}
          title={t('common.confirm')}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-2xs font-medium text-danger transition-colors hover:bg-danger/10 disabled:opacity-50"
        >
          <Icon name="check" size={12} />
        </button>
        <button
          type="button"
          onClick={() => setConfirming(false)}
          disabled={busy}
          title={t('common.cancel')}
          className="inline-flex items-center rounded-md px-1.5 py-0.5 text-2xs text-muted transition-colors hover:bg-surface-2 hover:text-text-2 disabled:opacity-50"
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
      className="inline-flex items-center gap-1 whitespace-nowrap rounded-md px-1.5 py-0.5 text-2xs text-muted transition-colors hover:bg-danger/10 hover:text-danger disabled:cursor-not-allowed disabled:opacity-40"
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
  const { t, locale } = useI18n()
  const time = fmtTime(item.ts, locale)
  return (
    <div className="group flex justify-end">
      <div className="relative max-w-[85%]">
        {/* 纯图片占位项无对应历史行，无法定位删除目标，不渲染删除按钮。 */}
        {!item.placeholder && (
          <div className="absolute right-full top-0 mr-1 flex items-center whitespace-nowrap opacity-100 sm:opacity-0 sm:transition-opacity sm:group-hover:opacity-100 sm:focus-within:opacity-100">
            <MessageDeleteButton item={item} confirmKey="transcript.delete_confirm_user" />
          </div>
        )}
        <div className="rounded-2xl rounded-br-md bg-primary px-4 py-2.5 text-[14px] leading-relaxed text-white dark:text-[#06241f]">
          <p className="whitespace-pre-wrap break-words">{item.text}</p>
        </div>
        {(item.steered || time) && (
          <p className="mt-1 flex items-center justify-end gap-1.5 text-2xs text-muted">
            {time && <span className="tabular">{time}</span>}
            {item.steered && (
              <>
                <Icon name="zap" size={11} />
                {t('transcript.steered')}
              </>
            )}
          </p>
        )}
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
function AssistantMessage({
  item,
  showAvatar = true,
}: {
  item: Extract<TranscriptItem, { kind: 'assistant' }>
  showAvatar?: boolean
}) {
  const { t, locale } = useI18n()
  const [copied, setCopied] = useState(false)
  const hasText = item.text.trim().length > 0
  const time = fmtTime(item.ts, locale)

  const copy = async () => {
    if (!hasText) return
    // copyText 兼容非安全上下文（http 部署）；仅在成功时反馈「已复制」，失败不假装成功。
    if (await copyText(item.text)) {
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    }
  }

  return (
    <div className="group flex gap-3">
      {showAvatar ? (
        <Avatar />
      ) : (
        // 占位与头像同宽，保持后续消息文本列对齐。
        <span className="h-8 w-8 shrink-0" aria-hidden />
      )}
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
            {time && <span className="tabular text-2xs text-muted/70">{time}</span>}
            <button
              type="button"
              onClick={copy}
              title={copied ? t('common.copied') : t('common.copy')}
              aria-label={copied ? t('common.copied') : t('common.copy')}
              className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-2xs text-muted transition-colors hover:bg-surface-2 hover:text-text-2"
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

function ThinkingBlock({
  text,
  streaming,
  ts,
}: {
  text: string
  streaming: boolean
  ts: number
}) {
  // 默认只在流式输出时展开；结束后自动收起（用户手动切换过则尊重其选择）。
  const [open, setOpen] = useState(streaming)
  const touched = useRef(false)
  const { t, locale } = useI18n()
  useEffect(() => {
    if (!streaming && !touched.current) setOpen(false)
  }, [streaming])
  // 折叠时的单行摘要：取首段非空文本，长内容截断。
  const preview = useMemo(
    () => text.split('\n').map((l) => l.trim()).find(Boolean) ?? '',
    [text],
  )
  const time = fmtTime(ts, locale)
  return (
    <div className="rounded-xl border border-border bg-surface-2/50">
      <button
        onClick={() => {
          touched.current = true
          setOpen((o) => !o)
        }}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-2 text-xs font-medium text-muted"
      >
        <Icon name="activity" size={14} className={streaming ? 'shrink-0 animate-pulse text-primary' : 'shrink-0'} />
        {t('transcript.thinking')}
        {streaming && <Badge tone="primary" className="px-1.5 py-0 text-2xs">{t('transcript.generating')}</Badge>}
        {!open && preview && (
          <span className="min-w-0 flex-1 truncate text-left font-normal text-muted/70">{preview}</span>
        )}
        {open && <span className="flex-1" />}
        {time && <span className="tabular shrink-0 text-2xs font-normal text-muted/60">{time}</span>}
        <Icon name="chevron-down" size={14} className={cn('shrink-0 transition-transform', open && 'rotate-180')} />
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

function ToolBlock({
  name,
  command,
  output,
  ts,
}: {
  name: string
  command?: string
  output: string
  ts: number
}) {
  const [open, setOpen] = useState(false)
  const { locale } = useI18n()
  const hasOutput = output.trim().length > 0
  const time = fmtTime(ts, locale)
  return (
    <div className="rounded-xl border border-border bg-surface-2/50">
      <button
        onClick={() => (hasOutput || !!command) && setOpen((o) => !o)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-2 text-xs"
      >
        <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-primary/10 text-primary">
          <Icon name="cpu" size={13} />
        </span>
        <span className="font-mono font-medium text-text-2">{name}</span>
        {/* 被执行的命令/主操作数：yolo 等无审批帧的模式下也能一眼看到工具在做什么。 */}
        {command && (
          <code
            className="min-w-0 flex-1 truncate font-mono text-2xs text-muted"
            title={command}
          >
            {command}
          </code>
        )}
        {time && <span className="tabular ml-auto shrink-0 text-2xs text-muted/60">{time}</span>}
        {hasOutput && (
          <Icon
            name="chevron-down"
            size={13}
            className={cn('shrink-0 text-muted transition-transform', open && 'rotate-180')}
          />
        )}
      </button>
      {open && hasOutput && (
        <pre className="max-h-72 overflow-auto border-t border-border bg-code-bg p-3 text-[12px] leading-relaxed text-code-fg">
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
  // 在途保护：审批/回复帧已发出但服务端 resolved 回执未达时，禁止二次提交。
  const [sent, setSent] = useState(false)
  const isFollowup = typeof ask.kind === 'string' && ask.kind === 'followup'

  const send = (response: Parameters<typeof respond>[1]) => {
    if (sent || resolved) return
    setSent(true)
    respond(ask.id, response)
  }

  return (
    <div role="alert" className="rounded-xl border border-primary/30 bg-primary/[0.04] p-3.5 shadow-soft">
      <div className="mb-2 flex items-center gap-2">
        <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-primary/15 text-primary">
          <Icon name="shield" size={15} />
        </span>
        <Badge tone="info">{askKindLabel(ask.kind, t)}</Badge>
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
            aria-label={t('transcript.reply_placeholder')}
            placeholder={t('transcript.reply_placeholder')}
            className="max-h-32 flex-1 resize-none rounded-lg border border-border bg-surface px-3 py-2 text-[13px] text-text placeholder:text-muted/60 focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/15"
          />
          <Button
            variant="primary"
            size="md"
            leftIcon="arrow-right"
            disabled={!text.trim() || sent}
            onClick={() => send({ text: text.trim() })}
          >
            {t('transcript.reply')}
          </Button>
        </div>
      ) : (
        <div className="flex gap-2">
          <Button variant="primary" leftIcon="check" disabled={sent} onClick={() => send('yes')}>
            {t('transcript.approve')}
          </Button>
          <Button
            variant="outline"
            leftIcon="close"
            className="text-danger hover:bg-danger/10"
            disabled={sent}
            onClick={() => send('no')}
          >
            {t('transcript.reject')}
          </Button>
        </div>
      )}
    </div>
  )
}
