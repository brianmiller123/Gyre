import { useState } from 'react'
import { Badge, Button, Dropdown, Modal } from '@/components/ui'
import { Icon } from '@/components/icons'
import { Sidebar } from '@/components/agent/Sidebar'
import { Transcript } from '@/components/agent/Transcript'
import { Composer } from '@/components/agent/Composer'
import { Inspector } from '@/components/agent/Inspector'
import { SettingsPanel } from '@/components/agent/SettingsPanel'
import { WorkspacePanel } from '@/components/agent/WorkspacePanel'
import { Toaster } from '@/components/Toaster'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { stateMeta } from '@/lib/agent/ui'
import { compact } from '@/lib/format'
import { useI18n } from '@/lib/i18n'

/** Full application frame: sidebar + chat column + inspector + overlays. */
export function AgentShell() {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [mobileNav, setMobileNav] = useState(false)
  const [inspectorOpen, setInspectorOpen] = useState(false)
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  const [confirmModel, setConfirmModel] = useState<string | null>(null)

  const { state, usage, error, running, stopping, clear, cancel, items, models, currentModel, switchModel } =
    useAgentSession()
  const { t } = useI18n()
  const meta = stateMeta[state as string] ?? stateMeta.no_task
  const totalTokens = usage.input_tokens + usage.output_tokens

  const requestSwitchModel = (alias: string | null) => {
    if (items.length > 0 && alias !== (currentModel?.alias ?? null)) {
      setConfirmModel(alias)
    } else {
      switchModel(alias)
    }
  }

  return (
    <div className="app-aurora relative flex h-screen overflow-hidden text-text">
      {/* Desktop sidebar */}
      <div className="hidden lg:block">
        <Sidebar
          onOpenSettings={() => setSettingsOpen(true)}
          onOpenWorkspace={() => setWorkspaceOpen(true)}
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
              onClose={() => setMobileNav(false)}
            />
          </div>
        </div>
      )}

      {/* Main column */}
      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar
          stateLabel={meta.label}
          stateTone={meta.tone}
          stateDot={meta.dot}
          totalTokens={totalTokens}
          cost={usage.cost_usd}
          running={running}
          stopping={stopping}
          models={models}
          currentModel={currentModel}
          onPickModel={requestSwitchModel}
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

      {workspaceOpen && <WorkspacePanel onClose={() => setWorkspaceOpen(false)} />}

      <Modal
        open={confirmModel !== null}
        onClose={() => setConfirmModel(null)}
        title={t('shell.switch_model')}
        description={t('shell.switch_model_desc')}
        icon="cube"
        size="sm"
        footer={
          <>
            <Button variant="secondary" onClick={() => setConfirmModel(null)}>
              {t('shell.cancel')}
            </Button>
            <Button
              variant="primary"
              leftIcon="check"
              onClick={() => {
                if (confirmModel !== null) switchModel(confirmModel)
                setConfirmModel(null)
              }}
            >
              {t('shell.switch_and_new')}
            </Button>
          </>
        }
      >
        <p className="text-sm text-text-2">
          {t('shell.switch_confirm_body', { model: confirmModel ?? t('shell.default') })}
        </p>
      </Modal>

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
  models,
  currentModel,
  onPickModel,
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
  models: Array<{ alias: string; id: string }>
  currentModel: { alias: string; id: string } | null
  onPickModel: (alias: string | null) => void
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

      {models.length > 0 && (
        <ModelMenu models={models} currentModel={currentModel} onPick={onPickModel} />
      )}

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

/** Compact model switcher. Index 0 is the server default (sent as model=null). */
function ModelMenu({
  models,
  currentModel,
  onPick,
}: {
  models: Array<{ alias: string; id: string }>
  currentModel: { alias: string; id: string } | null
  onPick: (alias: string | null) => void
}) {
  const { t } = useI18n()
  return (
    <Dropdown
      align="left"
      panelClassName="min-w-[15rem]"
      trigger={
        <button className="flex h-9 max-w-[180px] items-center gap-1.5 rounded-lg border border-border bg-surface-2/70 px-2.5 text-xs font-medium text-text-2 transition-colors hover:border-border-strong hover:text-text">
          <Icon name="cube" size={15} className="shrink-0 text-primary" />
          <span className="truncate">{currentModel?.alias ?? t('shell.default_model')}</span>
          <Icon name="chevron-down" size={13} className="shrink-0 text-muted" />
        </button>
      }
      items={models.map((m, i) => ({
        label: `${m.alias}  ·  ${m.id}`,
        active: currentModel?.alias === m.alias,
        onClick: () => onPick(i === 0 ? null : m.alias),
      }))}
    />
  )
}
