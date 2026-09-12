import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  Badge,
  Button,
  ConfirmDialog,
  Dropdown,
  IconButton,
  Kbd,
  useDialogA11y,
  type Tone,
} from '@/components/ui'
import { Icon } from '@/components/icons'
import { Sidebar } from '@/components/agent/Sidebar'
import { Transcript } from '@/components/agent/Transcript'
import { Composer } from '@/components/agent/Composer'
import { Inspector } from '@/components/agent/Inspector'
import { SettingsPanel } from '@/components/agent/SettingsPanel'
import { StatisticsPanel } from '@/components/agent/StatisticsPanel'
import { WorkspacePanel } from '@/components/agent/WorkspacePanel'
import { CommandPalette, type AppAction } from '@/components/agent/CommandPalette'
import { PanelBoundary } from '@/components/ErrorBoundary'
import { Toaster } from '@/components/Toaster'
import { useAgentSession, useTranscriptItems } from '@/lib/agent/useAgentSession'
import type { SessionActivity } from '@/lib/agent/types'
import { stateMeta } from '@/lib/agent/ui'
import { compact } from '@/lib/format'
import { useI18n } from '@/lib/i18n'
import { useTheme } from '@/lib/theme'
import { cn } from '@/lib/cn'

/** 统计页面的 URL 路由锚点（应用无路由库，用 `#/stats` hash 表达页面级导航）。 */
const STATS_ROUTE = '#/stats'

/** 运行面板停靠状态的持久化键（与其余偏好一致走 localStorage）。 */
const DOCK_KEY = 'agent-inspector-docked'
const XL_QUERY = '(min-width: 1280px)'

/** 平台修饰键标签：⌘ / Ctrl。仅用于提示文案。 */
const MOD_KEY =
  typeof navigator !== 'undefined' && /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent)
    ? '⌘'
    : 'Ctrl'

export type SettingsTab = 'connection' | 'appearance'

/**
 * 应用外壳：左栏 + 对话列 + 运行面板 + 浮层。
 *
 * 信息架构（v2）
 * --------------
 * 三条正交轴，各自只有一个归属地，杜绝重复入口：
 *
 *   · **会话生命周期**（新建/清空/停止） → 顶栏 ⋮ 菜单 + ⌘K + 斜杠命令
 *   · **视图切换**（文件/统计/运行面板）  → 侧栏导航 + 顶栏遥测按钮 + ⌘K
 *   · **偏好设置**（连接/外观）           → 唯一的设置面板（分标签页）+ ⌘K
 *
 * 所有动作都先在 `actions` 里登记一次，再由侧栏、顶栏溢出菜单、命令面板三处
 * 消费同一份数据；"某功能只在某处存在"或"同一动作两套文案"在结构上不可能出现。
 */
export function AgentShell() {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [settingsTab, setSettingsTab] = useState<SettingsTab>('connection')
  const [mobileNav, setMobileNav] = useState(false)
  const [inspectorDrawer, setInspectorDrawer] = useState(false)
  const [inspectorDocked, setInspectorDocked] = useState(() => {
    try {
      return localStorage.getItem(DOCK_KEY) !== '0'
    } catch {
      return true
    }
  })
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [confirmClear, setConfirmClear] = useState(false)
  // 统计页：与 URL hash 同步（`#/stats` 直达 / 可刷新 / 可分享）。
  const [statsOpen, setStatsOpen] = useState(() => window.location.hash === STATS_ROUTE)

  useEffect(() => {
    const onHash = () => setStatsOpen(window.location.hash === STATS_ROUTE)
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  useEffect(() => {
    try {
      localStorage.setItem(DOCK_KEY, inspectorDocked ? '1' : '0')
    } catch {
      /* storage 不可用时忽略：停靠状态退化为「本次会话有效」 */
    }
  }, [inspectorDocked])

  // 抽屉的 dialog 语义（焦点陷阱 + Esc + 锁滚动）——hook 必须无条件调用。
  const mobileNavA11y = useDialogA11y(mobileNav, () => setMobileNav(false))
  const inspectorA11y = useDialogA11y(inspectorDrawer, () => setInspectorDrawer(false))

  const { state, usage, error, running, stopping, activity, clear, cancel, connect, sessionId, newChat } =
    useAgentSession()
  const { t, locale } = useI18n()
  const { theme, toggle: toggleTheme } = useTheme()
  const meta = stateMeta[state as string] ?? stateMeta.no_task
  const totalTokens = usage.input_tokens + usage.output_tokens

  const openSettings = useCallback((tab: SettingsTab = 'connection') => {
    setSettingsTab(tab)
    setSettingsOpen(true)
  }, [])
  const openWorkspace = useCallback(() => setWorkspaceOpen(true), [])
  const openStats = useCallback(() => {
    window.location.hash = STATS_ROUTE
    setStatsOpen(true)
  }, [])
  const closeStats = useCallback(() => {
    setStatsOpen(false)
    // 清除 hash 但不触发 hashchange（避免重复 setState）。
    if (window.location.hash === STATS_ROUTE) {
      history.replaceState(null, '', window.location.pathname + window.location.search)
    }
  }, [])

  /**
   * 运行面板：宽屏下是「停靠/收起」开关，窄屏下是抽屉。
   * 单一按钮、单一语义，避免同一位置出现两个只差断点的控件。
   */
  const toggleInspector = useCallback(() => {
    const wide = typeof window !== 'undefined' && window.matchMedia(XL_QUERY).matches
    if (wide) setInspectorDocked((v) => !v)
    else setInspectorDrawer(true)
  }, [])

  /** 唯一的动作注册表：侧栏导航 / 顶栏溢出菜单 / ⌘K 面板共用同一份数据。 */
  const actions = useMemo<AppAction[]>(() => {
    const list: AppAction[] = [
      {
        id: 'new-chat',
        label: t('action.new_chat'),
        hint: t('action.new_chat_hint'),
        icon: 'plus',
        group: 'session',
        keywords: 'new session chat 新建 会话',
        run: newChat,
      },
    ]
    // 运行中才出现的动作：与其常驻禁用（噪声），不如按可用性出现。
    if (running) {
      list.push({
        id: 'stop',
        label: stopping ? t('shell.stopping') : t('action.stop'),
        hint: t('action.stop_hint'),
        icon: 'square',
        group: 'session',
        keywords: 'stop cancel abort 停止 取消',
        run: cancel,
      })
    }
    list.push(
      {
        id: 'clear',
        label: t('action.clear'),
        hint: t('action.clear_hint'),
        icon: 'trash',
        group: 'session',
        keywords: 'clear reset conversation 清空 重置',
        danger: true,
        run: () => setConfirmClear(true),
      },
      {
        id: 'files',
        label: t('action.files'),
        hint: t('action.files_hint'),
        icon: 'folder',
        group: 'view',
        keywords: 'files browse workspace 文件 浏览',
        run: openWorkspace,
      },
      {
        id: 'stats',
        label: t('action.stats'),
        hint: t('action.stats_hint'),
        icon: 'bar-chart',
        group: 'view',
        keywords: 'statistics usage trend 统计 用量',
        run: openStats,
      },
      {
        id: 'run-panel',
        label: t('action.run_panel'),
        hint: t('action.run_panel_hint'),
        icon: 'panel-right',
        group: 'view',
        keywords: 'inspector panel tokens context 运行面板 遥测',
        run: toggleInspector,
      },
      {
        id: 'theme',
        label: theme === 'dark' ? t('action.theme_light') : t('action.theme_dark'),
        hint: t('action.theme_hint'),
        icon: theme === 'dark' ? 'sun' : 'moon',
        group: 'preference',
        shortcut: `${MOD_KEY}⇧L`,
        keywords: 'theme dark light appearance 主题 深色 浅色',
        run: toggleTheme,
      },
      {
        id: 'language',
        label: t('action.language'),
        hint: t(`lang.${locale}`),
        icon: 'globe',
        group: 'preference',
        keywords: 'language locale i18n 语言',
        run: () => openSettings('appearance'),
      },
      {
        id: 'settings',
        label: t('action.settings'),
        hint: t('action.settings_hint'),
        icon: 'settings',
        group: 'preference',
        keywords: 'settings connection token 设置 连接 令牌',
        run: () => openSettings('connection'),
      },
    )
    return list
  }, [
    t,
    theme,
    locale,
    running,
    stopping,
    newChat,
    cancel,
    openWorkspace,
    openStats,
    openSettings,
    toggleInspector,
    toggleTheme,
  ])

  // 全局快捷键：⌘K 命令面板（统一入口）；⌘⇧L 主题开关。
  // 只绑定这两个——主题此前靠侧栏常驻按钮实现「1 击可达」，收敛到设置面板后
  // 用快捷键补回快捷路径，避免"去重"反噬为"变慢"。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey)) return
      const key = e.key.toLowerCase()
      if (key === 'k' && !e.shiftKey) {
        e.preventDefault()
        setPaletteOpen((v) => !v)
      } else if (key === 'l' && e.shiftKey) {
        e.preventDefault()
        toggleTheme()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [toggleTheme])

  return (
    <div className="app-aurora relative flex h-screen overflow-hidden text-text">
      {/* 桌面侧栏 */}
      <div className="hidden lg:block">
        <PanelBoundary label={t('sidebar.history')}>
          <Sidebar actions={actions} onOpenSettings={openSettings} />
        </PanelBoundary>
      </div>

      {/* 移动端侧栏抽屉 */}
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
            <PanelBoundary label={t('sidebar.history')}>
              <Sidebar
                actions={actions}
                onOpenSettings={(tab) => {
                  openSettings(tab)
                  setMobileNav(false)
                }}
                onClose={() => setMobileNav(false)}
              />
            </PanelBoundary>
          </div>
        </div>
      )}

      {/* 主列 */}
      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar
          stateLabel={t(meta.label)}
          stateTone={meta.tone}
          stateDot={meta.dot}
          totalTokens={totalTokens}
          cost={usage.cost_usd}
          activity={activity}
          modKey={MOD_KEY}
          actions={actions}
          inspectorOpen={inspectorDocked || inspectorDrawer}
          onMenu={() => setMobileNav(true)}
          onPalette={() => setPaletteOpen(true)}
          onInspector={toggleInspector}
        />

        {error && <ErrorBanner message={error} onRetry={() => connect(sessionId)} />}

        <div className="flex min-h-0 flex-1">
          <main className="flex min-w-0 flex-1 flex-col">
            <div className="min-h-0 flex-1">
              <PanelBoundary label={t('shell.conversation')}>
                <Transcript />
              </PanelBoundary>
            </div>
            <Composer onOpenSettings={openSettings} onOpenWorkspace={openWorkspace} />
          </main>

          {/* 桌面运行面板（可停靠 / 收起） */}
          {inspectorDocked && (
            <div className="hidden xl:block">
              <PanelBoundary label={t('shell.run_panel')}>
                <Inspector onClose={() => setInspectorDocked(false)} />
              </PanelBoundary>
            </div>
          )}
        </div>
      </div>

      {/* 窄屏运行面板抽屉 */}
      {inspectorDrawer && (
        <div className="fixed inset-0 z-drawer xl:hidden">
          <div className="app-backdrop" onClick={() => setInspectorDrawer(false)} />
          <div
            ref={inspectorA11y.ref}
            onKeyDown={inspectorA11y.onKeyDown}
            role="dialog"
            aria-modal="true"
            aria-label={t('shell.run_panel')}
            tabIndex={-1}
            className="absolute right-0 top-0 h-full w-80 animate-slide-right outline-none"
          >
            <PanelBoundary label={t('shell.run_panel')}>
              <Inspector onClose={() => setInspectorDrawer(false)} />
            </PanelBoundary>
          </div>
        </div>
      )}

      <CommandPalette open={paletteOpen} onClose={() => setPaletteOpen(false)} actions={actions} />

      <SettingsPanel
        open={settingsOpen}
        tab={settingsTab}
        onTabChange={setSettingsTab}
        onClose={() => setSettingsOpen(false)}
      />

      {statsOpen && (
        <PanelBoundary label={t('stats.title')}>
          <StatisticsPanel onClose={closeStats} />
        </PanelBoundary>
      )}

      {workspaceOpen && (
        <PanelBoundary label={t('workspace.browse')}>
          <WorkspacePanel onClose={() => setWorkspaceOpen(false)} />
        </PanelBoundary>
      )}

      {/* 清空对话的唯一常驻入口在顶栏 ⋮（会话生命周期），这里是它唯一的确认框。 */}
      <ConfirmDialog
        open={confirmClear}
        onClose={() => setConfirmClear(false)}
        onConfirm={clear}
        title={t('action.clear')}
        body={t('shell.clear_confirm_body')}
        confirmLabel={t('action.clear')}
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
    <div className="flex items-start gap-2 border-b border-danger/25 bg-danger/10 px-4 py-2 text-xs text-danger">
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
        className="focus-ring shrink-0 rounded px-1 font-medium transition-colors hover:bg-danger/15"
      >
        {expanded ? t('common.collapse') : t('common.expand')}
      </button>
      <Button
        size="sm"
        variant="outline"
        leftIcon="refresh"
        onClick={onRetry}
        className="h-6 shrink-0 border-danger/40 px-2 text-danger hover:bg-danger/15"
      >
        {t('common.retry')}
      </Button>
    </div>
  )
}

/**
 * 顶栏：身份 + 状态 + 两个全局入口 + 会话溢出菜单。
 *
 * 精简依据（对比旧版）：旧顶栏同时存在 token 胶囊与 gauge 图标（同一面板两个
 * 入口）、以及顶栏 Stop 与输入框 Stop（同一中断两个入口）。这里各收敛为一个：
 * 遥测按钮在所有断点唯一存在（窄屏只显示图标），中断只保留输入框内的那一个。
 */
function TopBar({
  stateLabel,
  stateTone,
  stateDot,
  totalTokens,
  cost,
  activity,
  modKey,
  actions,
  inspectorOpen,
  onMenu,
  onPalette,
  onInspector,
}: {
  stateLabel: string
  stateTone: Tone
  stateDot?: boolean
  totalTokens: number
  cost: number
  activity: SessionActivity | null
  modKey: string
  actions: AppAction[]
  inspectorOpen: boolean
  onMenu: () => void
  onPalette: () => void
  onInspector: () => void
}) {
  const { t } = useI18n()
  // 会话标题：首条非空用户消息（截断）。空会话回退到「对话」。
  // 顺带同步 document.title，多会话多标签页时可辨认。
  const items = useTranscriptItems()
  const title = useMemo(() => {
    for (const it of items) {
      if (it.kind === 'user' && it.text.trim()) {
        return it.text.replace(/\s+/g, ' ').trim().slice(0, 80)
      }
    }
    return ''
  }, [items])
  useEffect(() => {
    document.title = title ? `${title} · Agent · Console` : 'Agent · Console'
  }, [title])

  // 溢出菜单只承载会话生命周期动作（与侧栏的「视图 / 偏好」互不重叠）。
  const sessionItems = actions
    .filter((a) => a.group === 'session')
    .map((a) => ({
      label: a.label,
      icon: a.icon,
      danger: a.danger,
      disabled: a.disabled,
      onClick: a.run,
    }))

  return (
    <header className="glass-bar relative z-header flex h-14 shrink-0 items-center gap-2 border-b px-3 sm:gap-3 sm:px-5">
      <IconButton icon="menu" label={t('shell.menu')} onClick={onMenu} className="lg:hidden" />

      {/* 会话身份区：真正的会话标题占 h1，固定文案只作为空会话的占位。
          旧版把「对话」当主标题、把会话标题降级为灰字，层级正好倒置。 */}
      <div className="flex min-w-0 items-center gap-2.5">
        <h1
          className={cn(
            'min-w-0 truncate font-display text-md font-semibold',
            title ? 'text-text' : 'text-muted',
          )}
          title={title || undefined}
        >
          {title || t('shell.conversation')}
        </h1>

        <Badge tone={stateTone} dot={stateDot} className="hidden shrink-0 sm:inline-flex">
          {stateLabel}
        </Badge>

        {activity && (
          <Badge tone="warning" dot className="hidden shrink-0 sm:inline-flex" aria-live="polite">
            {activity.kind === 'retry'
              ? t('shell.retrying', { attempt: activity.attempt, max: activity.maxAttempts })
              : t('shell.compacting')}
          </Badge>
        )}
      </div>

      <div className="flex-1" />

      {/* 统一入口：⌘K 命令面板（宽屏带键帽提示，窄屏降级为图标）。 */}
      <button
        type="button"
        onClick={onPalette}
        title={t('palette.title')}
        aria-label={t('palette.title')}
        className="focus-ring hidden h-8 items-center gap-2 rounded-lg border border-border bg-surface-2 px-2.5 text-xs text-muted transition-colors hover:border-border-strong hover:text-text sm:flex"
      >
        <Icon name="search" size={14} />
        <span>{t('palette.trigger')}</span>
        <Kbd>{modKey}K</Kbd>
      </button>
      <IconButton icon="search" label={t('palette.title')} onClick={onPalette} className="sm:hidden" />

      {/* 遥测入口：所有断点唯一存在。宽屏胶囊附带 token/成本上下文，窄屏收成
          图标；点击在宽屏切换运行面板停靠，窄屏打开抽屉，且以高亮回馈当前态。 */}
      <button
        type="button"
        onClick={onInspector}
        title={t('action.run_panel_hint')}
        aria-pressed={inspectorOpen}
        aria-label={t('shell.run_panel')}
        className={cn(
          'focus-ring flex h-8 shrink-0 items-center gap-1.5 rounded-lg border px-2 text-xs transition-colors sm:px-2.5',
          inspectorOpen
            ? 'border-primary/40 bg-primary/10 text-primary'
            : 'border-border bg-surface-2 text-muted hover:border-border-strong hover:text-text',
        )}
      >
        <Icon name="activity" size={14} />
        {totalTokens > 0 && (
          <>
            <span className="tabular hidden font-medium sm:inline">{compact(totalTokens)}</span>
            <span className="hidden sm:inline">tokens</span>
            {cost > 0 && <span className="tabular hidden text-muted sm:inline">· ${cost.toFixed(4)}</span>}
          </>
        )}
      </button>

      <Dropdown
        align="right"
        trigger={<IconButton icon="dots" label={t('shell.session_actions')} />}
        items={sessionItems}
      />
    </header>
  )
}
