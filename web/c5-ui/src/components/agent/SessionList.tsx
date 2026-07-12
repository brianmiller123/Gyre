import { useEffect, useMemo, useRef, useState } from 'react'
import type { SessionListItem } from '@/lib/agent/types'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useNotifications } from '@/lib/notifications'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/cn'
import { Icon } from '@/components/icons'
import { Button, Dropdown, EmptyState, Modal, Skeleton } from '@/components/ui'

/**
 * 历史会话管理列表：侧栏主区域。
 *
 * 职责（与 useAgentSession 联动，数据实时同步、局部动态更新，无需刷新页面）：
 * - 搜索过滤 + 按时间分组（今天 / 昨天 / 近 7 天 / 更早）展示全部历史与当前会话
 * - 切换会话（点击行）、新建会话（空状态入口）
 * - 内联重命名（行内 input，Enter 保存 / Esc 取消 / blur 保存）
 * - 删除会话（二次确认 Modal，防误触）+ 操作完成 Toast 反馈
 * - 边界态：加载骨架屏、加载失败重试、无会话空状态、搜索无结果
 */
export function SessionList({ onClose }: { onClose?: () => void }) {
  const {
    sessions,
    sessionId,
    sessionsLoading,
    sessionsError,
    switchSession,
    refreshSessions,
    deleteSession,
    renameSession,
    newChat,
  } = useAgentSession()
  const { toast } = useNotifications()
  const { t } = useI18n()

  const [query, setQuery] = useState('')
  const [renamingId, setRenamingId] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [confirmDel, setConfirmDel] = useState<SessionListItem | null>(null)
  const [busy, setBusy] = useState(false)
  const renameRef = useRef<HTMLInputElement>(null)

  // 进入重命名时自动聚焦并全选文本。
  useEffect(() => {
    if (renamingId && renameRef.current) {
      renameRef.current.focus()
      renameRef.current.select()
    }
  }, [renamingId])

  const display = (s: SessionListItem) => s.title || s.preview

  // 关键词过滤（标题或首条预览）。
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return sessions
    return sessions.filter((s) => display(s).toLowerCase().includes(q))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessions, query])

  // 按时间桶分组（保持原列表的按修改时间倒序）。
  const groups = useMemo(() => {
    const g: Record<Bucket, SessionListItem[]> = { today: [], yesterday: [], week: [], older: [] }
    for (const s of filtered) g[bucketOf(s.mtime_ms)].push(s)
    return g
  }, [filtered])

  const startRename = (s: SessionListItem) => {
    setRenamingId(s.id)
    setDraft(s.title || s.preview)
  }

  const commitRename = async () => {
    const id = renamingId
    if (!id) return
    const current = sessions.find((s) => s.id === id)
    const next = draft.trim()
    setRenamingId(null)
    // 空标题或未改动：直接取消，不发起请求。
    if (!next || !current || next === (current.title || current.preview)) return
    setBusy(true)
    const res = await renameSession(id, next)
    setBusy(false)
    if (res.ok) toast({ title: t('sessions.renamed'), severity: 'success' })
    else toast({ title: t('sessions.rename_failed'), body: res.error, severity: 'danger' })
  }

  const doDelete = async () => {
    if (!confirmDel) return
    const target = confirmDel
    setBusy(true)
    const res = await deleteSession(target.id)
    setBusy(false)
    if (res.ok) {
      toast({ title: t('sessions.deleted'), body: display(target), severity: 'success' })
      onClose?.()
    } else {
      toast({ title: t('sessions.delete_failed'), body: res.error, severity: 'danger' })
    }
    setConfirmDel(null)
  }

  const initialLoading = sessionsLoading && sessions.length === 0
  const loadFailed = !sessionsLoading && sessionsError && sessions.length === 0
  const isEmpty = !sessionsLoading && !sessionsError && sessions.length === 0
  const noMatch = sessions.length > 0 && filtered.length === 0

  return (
    <div className="flex min-h-0 flex-1 flex-col px-3 pb-1.5">
      {/* 标题栏：标题 + 计数 + 刷新 */}
      <div className="flex items-center justify-between px-1 pb-1.5 pt-0.5">
        <span className="flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-wide text-muted">
          {t('sessions.heading')}
          {sessions.length > 0 && (
            <span className="rounded-full bg-surface-3 px-1.5 text-[9px] tabular leading-[1.4] text-muted">
              {t('sessions.count', { n: sessions.length })}
            </span>
          )}
        </span>
        <button
          onClick={() => void refreshSessions()}
          title={t('sidebar.refresh')}
          className="flex h-5 w-5 items-center justify-center rounded text-muted transition-colors hover:bg-surface-2 hover:text-text"
        >
          <Icon name="refresh" size={12} className={sessionsLoading ? 'animate-spin' : ''} />
        </button>
      </div>

      {/* 搜索框（仅有会话时显示） */}
      {sessions.length > 0 && (
        <div className="relative mb-1.5">
          <Icon
            name="search"
            size={13}
            className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-muted"
          />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('sessions.search_placeholder')}
            className="h-8 w-full rounded-lg border border-border bg-surface-2 pl-8 pr-7 text-[12px] text-text outline-none transition-colors placeholder:text-muted/60 focus:border-primary focus:ring-2 focus:ring-primary/20"
          />
          {query && (
            <button
              onClick={() => setQuery('')}
              className="absolute right-1.5 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded text-muted transition-colors hover:bg-surface-3 hover:text-text"
              aria-label={t('sessions.clear_filter')}
            >
              <Icon name="close" size={12} />
            </button>
          )}
        </div>
      )}

      {/* 列表区（可滚动） */}
      <div className="no-scrollbar min-h-0 flex-1 overflow-y-auto pr-0.5">
        {initialLoading && (
          <div className="space-y-1 px-1 pt-1">
            {Array.from({ length: 5 }).map((_, i) => {
              const w = ['w-3/4', 'w-2/3', 'w-4/5', 'w-3/5', 'w-1/2'][i] ?? 'w-3/4'
              return (
                <div key={i} className="flex items-center gap-2 px-1 py-2">
                  <Skeleton className="h-1.5 w-1.5 rounded-full" />
                  <div className="flex-1 space-y-1.5">
                    <Skeleton className={cn('h-3 rounded', w)} />
                    <Skeleton className="h-2 w-10 rounded" />
                  </div>
                </div>
              )
            })}
          </div>
        )}

        {loadFailed && (
          <div className="flex flex-col items-center px-3 py-8 text-center">
            <span className="mb-2.5 flex h-11 w-11 items-center justify-center rounded-xl bg-danger/10 text-danger/80">
              <Icon name="alert" size={20} />
            </span>
            <p className="text-xs text-text-2">{t('sessions.error')}</p>
            <Button
              size="sm"
              variant="secondary"
              className="mt-3"
              leftIcon="refresh"
              onClick={() => void refreshSessions()}
            >
              {t('sessions.retry')}
            </Button>
          </div>
        )}

        {isEmpty && (
          <EmptyState
            icon="message-square"
            title={t('sessions.empty_title')}
            description={t('sessions.empty_desc')}
            className="py-8"
            action={
              <Button
                size="sm"
                variant="primary"
                leftIcon="plus"
                onClick={() => {
                  newChat()
                  onClose?.()
                }}
              >
                {t('sessions.empty_action')}
              </Button>
            }
          />
        )}

        {noMatch && (
          <div className="flex flex-col items-center px-3 py-8 text-center">
            <Icon name="search" size={18} className="text-muted" />
            <p className="mt-2 max-w-full truncate text-[11px] text-muted">{query}</p>
            <button
              onClick={() => setQuery('')}
              className="mt-1 text-[11px] font-medium text-primary hover:underline"
            >
              {t('sessions.clear_filter')}
            </button>
          </div>
        )}

        {!initialLoading && filtered.length > 0 && (
          <div className="space-y-2.5">
            {BUCKET_ORDER.map((b) =>
              groups[b].length === 0 ? null : (
                <div key={b}>
                  <div className="px-2 pb-1 pt-0.5 text-[9.5px] font-semibold uppercase tracking-wider text-muted/70">
                    {t(`sessions.bucket.${b}`)}
                  </div>
                  <div className="space-y-0.5">
                    {groups[b].map((s) => (
                      <SessionRow
                        key={s.id}
                        active={s.id === sessionId}
                        renaming={renamingId === s.id}
                        draft={draft}
                        busy={busy}
                        renameRef={renameRef}
                        onSwitch={() => {
                          switchSession(s.id)
                          onClose?.()
                        }}
                        onStartRename={() => startRename(s)}
                        onDraftChange={setDraft}
                        onCommitRename={() => void commitRename()}
                        onCancelRename={() => setRenamingId(null)}
                        onRequestDelete={() => setConfirmDel(s)}
                        display={display(s)}
                        timeLabel={formatRelative(s.mtime_ms, t)}
                      />
                    ))}
                  </div>
                </div>
              ),
            )}
          </div>
        )}
      </div>

      {/* 删除二次确认 */}
      <Modal
        open={confirmDel !== null}
        onClose={() => !busy && setConfirmDel(null)}
        title={t('sessions.delete_title')}
        size="sm"
        footer={
          <>
            <Button variant="secondary" onClick={() => setConfirmDel(null)} disabled={busy}>
              {t('sessions.cancel')}
            </Button>
            <Button variant="danger" leftIcon="trash" loading={busy} onClick={() => void doDelete()}>
              {t('sessions.delete_confirm')}
            </Button>
          </>
        }
      >
        <p className="text-sm leading-relaxed text-text-2">
          {confirmDel ? t('sessions.delete_desc', { name: display(confirmDel) }) : ''}
        </p>
      </Modal>
    </div>
  )
}

/* ------------------------------- 单行渲染 -------------------------------- */

function SessionRow({
  active,
  renaming,
  draft,
  busy,
  renameRef,
  onSwitch,
  onStartRename,
  onDraftChange,
  onCommitRename,
  onCancelRename,
  onRequestDelete,
  display,
  timeLabel,
}: {
  active: boolean
  renaming: boolean
  draft: string
  busy: boolean
  renameRef: React.RefObject<HTMLInputElement>
  onSwitch: () => void
  onStartRename: () => void
  onDraftChange: (v: string) => void
  onCommitRename: () => void
  onCancelRename: () => void
  onRequestDelete: () => void
  display: string
  timeLabel: string
}) {
  const { t } = useI18n()
  return (
    <div
      className={cn(
        'group relative flex items-center gap-0.5 rounded-lg pl-1.5 pr-0.5 transition-colors',
        active ? 'bg-primary/10' : 'hover:bg-surface-2',
      )}
    >
      {renaming ? (
        <>
          <span
            className={cn(
              'mt-[7px] h-1.5 w-1.5 shrink-0 rounded-full',
              active ? 'bg-primary' : 'bg-muted/50',
            )}
          />
          <div className="min-w-0 flex-1 py-1.5">
            <input
              ref={renameRef}
              value={draft}
              disabled={busy}
              maxLength={120}
              onChange={(e) => onDraftChange(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  onCommitRename()
                } else if (e.key === 'Escape') {
                  e.preventDefault()
                  onCancelRename()
                }
              }}
              onBlur={() => onCommitRename()}
              className="h-7 w-full rounded-md border border-primary bg-surface px-1.5 text-[12.5px] text-text outline-none ring-2 ring-primary/20 focus:ring-primary/30 disabled:opacity-60"
            />
            <span className="mt-0.5 block text-[9.5px] text-muted">{timeLabel}</span>
          </div>
        </>
      ) : (
        <>
          <button
            onClick={onSwitch}
            title={display}
            className="flex min-w-0 flex-1 items-start gap-2 py-1.5 pl-1 pr-1 text-left"
          >
            <span
              className={cn(
                'mt-[5px] h-1.5 w-1.5 shrink-0 rounded-full',
                active ? 'bg-primary' : 'bg-muted/50',
              )}
            />
            <span className="min-w-0 flex-1">
              <span
                className={cn(
                  'block truncate text-[12.5px] leading-tight',
                  active ? 'font-medium text-primary' : 'text-text-2',
                )}
              >
                {display}
              </span>
              <span className="mt-0.5 block text-[9.5px] text-muted">{timeLabel}</span>
            </span>
          </button>
          <Dropdown
            align="right"
            panelClassName="min-w-[10rem]"
            trigger={
              <button
                aria-label="更多操作"
                className={cn(
                  'flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-muted transition-all hover:bg-surface-3 hover:text-text',
                  active ? 'opacity-70' : 'opacity-0 focus:opacity-100 group-hover:opacity-100',
                )}
              >
                <Icon name="dots" size={15} />
              </button>
            }
            items={[
              { label: t('sessions.rename'), icon: 'edit', onClick: onStartRename },
              {
                label: t('sessions.delete'),
                icon: 'trash',
                danger: true,
                onClick: onRequestDelete,
              },
            ]}
          />
        </>
      )}
    </div>
  )
}

/* ------------------------------- 辅助函数 -------------------------------- */

type Bucket = 'today' | 'yesterday' | 'week' | 'older'
const BUCKET_ORDER: Bucket[] = ['today', 'yesterday', 'week', 'older']

/** 把毫秒时间戳归到时间桶。 */
function bucketOf(ms: number): Bucket {
  const now = new Date()
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()
  const day = 86_400_000
  if (ms >= startOfToday) return 'today'
  if (ms >= startOfToday - day) return 'yesterday'
  if (ms >= startOfToday - 7 * day) return 'week'
  return 'older'
}

/** 相对时间（毫秒 → 国际化文本）。 */
function formatRelative(
  ms: number,
  t: (key: string, args?: Record<string, string | number>) => string,
): string {
  const diff = Date.now() - ms
  if (diff < 60_000) return t('time.just_now')
  if (diff < 3_600_000) return t('time.minutes_ago', { n: Math.floor(diff / 60_000) })
  if (diff < 86_400_000) return t('time.hours_ago', { n: Math.floor(diff / 3_600_000) })
  return t('time.days_ago', { n: Math.floor(diff / 86_400_000) })
}
