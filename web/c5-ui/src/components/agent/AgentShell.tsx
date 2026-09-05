import { useEffect, useState } from 'react'
import { Badge, Button } from '@/components/ui'
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

/** 统计页面的 URL 路由锚点（应用无路由库，用 `#/stats` hash 表达页面级导航）。 */
const STATS_ROUTE = '#/stats'

/** Full application frame: sidebar + chat column + inspector + overlays. */
export function AgentShell() {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [mobileNav, setMobileNav] = useState(false)
  const [inspectorOpen, setInspectorOpen] = useState(false)
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  // 统计页：与 URL hash 同步（`#/stats` 直达 / 可刷新 / 可分享）。
  const [statsOpen, setStatsOpen] = useState(() => window.location.hash === STATS_ROUTE)
  useEffect(() => {
    const onHash = () => setStatsOpen(window.location.hash === STATS_ROUTE)
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

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

  const { state, usage, error, running, stopping, clear, cancel } = useAgentSession()
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
        <div className="fixed inset-0 z-[90] lg:hidden">
          <div className="absolute inset-0 bg-black/50 backdrop-blur-sm animate-fade-in" onClick={() => setMobileNav(false)} />
          <div className="absolute left-0 top-0 h-full animate-slide-left">
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
          onClear={clear}
          onCancel={cancel}
        />

        {error && (
          <div className="flex items-center gap-2 border-b border-danger/20 bg-danger/[0.06] px-4 py-2 text-xs text-danger">
            <Icon name="alert" size={14} className="shrink-0" />
            <span className="flex-1 truncate">{error}</span>
          </div>
        )}

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
        <div className="fixed inset-0 z-[90] xl:hidden">
          <div className="absolute inset-0 bg-black/50 backdrop-blur-sm animate-fade-in" onClick={() => setInspectorOpen(false)} />
          <div className="absolute right-0 top-0 h-full w-80 animate-slide-right">
            <Inspector onClose={() => setInspectorOpen(false)} />
          </div>
        </div>
      )}

      <SettingsPanel open={settingsOpen} onClose={() => setSettingsOpen(false)} />

      {statsOpen && <StatisticsPanel onClose={closeStats} />}

      {workspaceOpen && <WorkspacePanel onClose={() => setWorkspaceOpen(false)} />}

      <Toaster />
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
      <button
        onClick={onMenu}
        className="flex h-9 w-9 items-center justify-center rounded-lg text-text-2 hover:bg-surface-2 hover:text-text lg:hidden"
        aria-label={t('shell.menu')}
      >
        <Icon name="menu" size={20} />
      </button>

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

      <button
        onClick={onInspector}
        className="flex h-9 w-9 items-center justify-center rounded-lg text-text-2 hover:bg-surface-2 hover:text-text xl:hidden"
        aria-label={t('shell.run_panel')}
      >
        <Icon name="gauge" size={19} />
      </button>
    </header>
  )
}
