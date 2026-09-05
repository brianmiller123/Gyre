import { useEffect, useState } from 'react'
import { Badge, Button, ConfirmDialog, IconButton, useDialogA11y } from '@/components/ui'
import { Icon } from '@/components/icons'
import { Sidebar } from '@/components/agent/Sidebar'
import { Transcript } from '@/components/agent/Transcript'
import { Composer } from '@/components/agent/Composer'
import { Inspector } from '@/components/agent/Inspector'
import { SettingsPanel } from '@/components/agent/SettingsPanel'
import { StatisticsPanel } from '@/components/agent/StatisticsPanel'
import { WorkspacePanel } from '@/components/agent/WorkspacePanel'
import { Toaster } from '@/components/Toaster'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { stateMeta } from '@/lib/agent/ui'
import { compact } from '@/lib/format'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/cn'

/** 统计页面的 URL 路由锚点（应用无路由库，用 `#/stats` hash 表达页面级导航）。 */
const STATS_ROUTE = '#/stats'

/** Full application frame: sidebar + chat column + inspector + overlays. */
export function AgentShell() {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [mobileNav, setMobileNav] = useState(false)
  const [inspectorOpen, setInspectorOpen] = useState(false)
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  const [confirmClear, setConfirmClear] = useState(false)
  // 统计页：与 URL hash 同步（`#/stats` 直达 / 可刷新 / 可分享）。
  const [statsOpen, setStatsOpen] = useState(() => window.location.hash === STATS_ROUTE)
  useEffect(() => {
    const onHash = () => setStatsOpen(window.location.hash === STATS_ROUTE)
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  // 抽屉的 dialog 语义（焦点陷阱 + Esc + 锁滚动）——hook 必须无条件调用。
  const mobileNavA11y = useDialogA11y(mobileNav, () => setMobileNav(false))
  const inspectorA11y = useDialogA11y(inspectorOpen, () => setInspectorOpen(false))

  const openStats = () => {
    window.location.hash = STATS_ROUTE
    setStatsOpen(true)
  }
  const closeStats = () => {
    setStatsOpen(false)
    // 清除 hash 但不触发 hashchange（避免重复 setState）。
    if (window.location.hash === STATS_ROUTE) {
      history.replaceState(null, '', window.location.pathname + window.location.search)
    }
  }

  const { state, usage, error, running, stopping, clear, cancel, connect, sessionId } =
    useAgentSession()
  const { t } = useI18n()
  const meta = stateMeta[state as string] ?? stateMeta.no_task
  const totalTokens = usage.input_tokens + usage.output_tokens

  return (
    <div className="app-aurora relative flex h-screen overflow-hidden text-text">
      {/* Desktop sidebar */}
      <div className="hidden lg:block">
        <Sidebar
          onOpenSettings={() => setSettingsOpen(true)}
          onOpenWorkspace={() => setWorkspaceOpen(true)}
          onOpenStats={openStats}
        />
      </div>

      {/* Mobile sidebar drawer */}
      {mobileNav && (
        <div className="fixed inset-0 z-drawer lg:hidden">
          <div className="app-backdrop" onClick={() => setMobileNav(false)} />
          <div
            ref={mobileNavA11y.ref}
            onKeyDown={mobileNavA11y.onKeyDown}
            role="dialog"
            aria-modal="true"
            aria-label={t('shell.conversation')}
            tabIndex={-1}
            className="absolute left-0 top-0 h-full animate-slide-left outline-none"
          >
            <Sidebar
              onOpenSettings={() => { setSettingsOpen(true); setMobileNav(false) }}
              onOpenWorkspace={() => { setWorkspaceOpen(true); setMobileNav(false) }}
              onOpenStats={() => { openStats(); setMobileNav(false) }}
              onClose={() => setMobileNav(false)}
            />
          </div>
        </div>
      )}

      {/* Main column */}
      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar
          stateLabel={t(meta.label)}
          stateTone={meta.tone}
          stateDot={meta.dot}
          totalTokens={totalTokens}
          cost={usage.cost_usd}
          running={running}
          stopping={stopping}
          onMenu={() => setMobileNav(true)}
          onInspector={() => setInspectorOpen(true)}
          onClear={() => setConfirmClear(true)}
          onCancel={cancel}
        />

        {error && <ErrorBanner message={error} onRetry={() => connect(sessionId)} />}

        <div className="flex min-h-0 flex-1">
          <main className="flex min-w-0 flex-1 flex-col">
            <div className="min-h-0 flex-1">
              <Transcript />
            </div>
            <Composer
              onOpenSettings={() => setSettingsOpen(true)}
              onOpenWorkspace={() => setWorkspaceOpen(true)}
            />
          </main>

          {/* Desktop inspector */}
          <div className="hidden xl:block">
            <Inspector />
          </div>
        </div>
      </div>

      {/* Mobile inspector drawer */}
      {inspectorOpen && (
        <div className="fixed inset-0 z-drawer xl:hidden">
          <div className="app-backdrop" onClick={() => setInspectorOpen(false)} />
          <div
            ref={inspectorA11y.ref}
            onKeyDown={inspectorA11y.onKeyDown}
            role="dialog"
            aria-modal="true"
            aria-label={t('shell.run_panel')}
            tabIndex={-1}
            className="absolute right-0 top-0 h-full w-80 animate-slide-right outline-none"
          >
            <Inspector onClose={() => setInspectorOpen(false)} />
          </div>
        </div>
      )}

      <SettingsPanel open={settingsOpen} onClose={() => setSettingsOpen(false)} />

      {statsOpen && <StatisticsPanel onClose={closeStats} />}

      {workspaceOpen && <WorkspacePanel onClose={() => setWorkspaceOpen(false)} />}

      <ConfirmDialog
        open={confirmClear}
        onClose={() => setConfirmClear(false)}
        onConfirm={clear}
        title={t('shell.clear')}
        body={t('shell.clear_confirm_body')}
        confirmLabel={t('shell.clear')}
      />

      <Toaster />
    </div>
  )
}

/** 连接错误横幅：长错误可展开查看全文，并提供一键重连。 */
function ErrorBanner({ message, onRetry }: { message: string; onRetry: () => void }) {
  const { t } = useI18n()
  const [expanded, setExpanded] = useState(false)
  return (
    <div className="flex items-start gap-2 border-b border-danger/20 bg-danger/[0.06] px-4 py-2 text-xs text-danger">
      <Icon name="alert" size={14} className="mt-0.5 shrink-0" />
      <span
        className={cn('flex-1 text-danger', expanded ? 'whitespace-pre-wrap break-all' : 'truncate')}
        title={message}
      >
        {message}
      </span>
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        className="shrink-0 rounded px-1 font-medium transition-colors hover:bg-danger/10"
      >
        {expanded ? t('common.collapse') : t('common.expand')}
      </button>
      <Button
        size="sm"
        variant="outline"
        leftIcon="refresh"
        onClick={onRetry}
        className="h-6 shrink-0 border-danger/30 px-2 text-danger hover:bg-danger/10"
      >
        {t('common.retry')}
      </Button>
    </div>
  )
}

function TopBar({
  stateLabel,
  stateTone,
  stateDot,
  totalTokens,
  cost,
  running,
  stopping,
  onMenu,
  onInspector,
  onClear,
  onCancel,
}: {
  stateLabel: string
  stateTone: any
  stateDot?: boolean
  totalTokens: number
  cost: number
  running: boolean
  stopping: boolean
  onMenu: () => void
  onInspector: () => void
  onClear: () => void
  onCancel: () => void
}) {
  const { t } = useI18n()
  return (
    <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border bg-surface/70 px-3 backdrop-blur-xl sm:px-5">
      <IconButton
        icon="menu"
        label={t('shell.menu')}
        onClick={onMenu}
        className="lg:hidden"
      />

      <h1 className="font-display text-[15px] font-semibold text-text">{t('shell.conversation')}</h1>

      <Badge tone={stateTone} dot={stateDot} className="hidden sm:inline-flex">
        {stateLabel}
      </Badge>

      <div className="flex-1" />

      {totalTokens > 0 && (
        <div className="hidden items-center gap-1.5 rounded-lg border border-border bg-surface-2/60 px-2.5 py-1.5 text-xs text-muted sm:flex">
          <Icon name="activity" size={13} className="text-primary" />
          <span className="tabular font-medium text-text-2">{compact(totalTokens)}</span>
          <span>tokens</span>
          {cost > 0 && <span className="tabular text-muted">· ${cost.toFixed(4)}</span>}
        </div>
      )}

      {running && (
        <Button
          size="sm"
          variant="ghost"
          className="text-danger hover:bg-danger/10"
          leftIcon={stopping ? undefined : 'square'}
          loading={stopping}
          onClick={onCancel}
        >
          <span className="hidden sm:inline">{stopping ? t('shell.stopping') : t('shell.stop')}</span>
        </Button>
      )}
      <Button size="sm" variant="ghost" leftIcon="trash" onClick={onClear} aria-label={t('shell.clear')}>
        <span className="hidden sm:inline">{t('shell.clear')}</span>
      </Button>

      <IconButton
        icon="gauge"
        label={t('shell.run_panel')}
        onClick={onInspector}
        className="xl:hidden"
      />
    </header>
  )
}
