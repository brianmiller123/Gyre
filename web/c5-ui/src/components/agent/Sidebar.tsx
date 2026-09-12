import { Button, Divider, IconButton } from '@/components/ui'
import { Icon } from '@/components/icons'
import { cn } from '@/lib/cn'
import { SessionList } from '@/components/agent/SessionList'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useSettings } from '@/lib/settings'
import { useI18n } from '@/lib/i18n'
import type { AppAction } from '@/components/agent/CommandPalette'
import type { SettingsTab } from '@/components/agent/AgentShell'

/**
 * 侧栏底部**视图导航**的固定顺序。
 *
 * 只收「打开一个独立视图」的动作（文件浏览器 / 统计）。运行面板是**面板开合**
 * 而非视图切换，它的唯一入口在顶栏遥测按钮；把两类混进同一列会重现旧版
 * 「四个图标平铺、导航/配置/破坏性操作混排」的问题。
 */
const NAV_IDS = ['files', 'stats']

/**
 * 左侧控制栏：品牌 → 主行动（新建对话）→ 会话历史 → 全局入口（导航/设置/状态）。
 *
 * 自上而下是「越往下越低频」的排序：会话历史占据唯一的弹性空间，全局与偏好类
 * 入口全部压在底部固定区。主题/语言快捷开关已移除——它们是**设一次**的偏好，
 * 唯一归属地是设置面板的外观标签页；需要快速切换时走 ⌘K（Toggle theme）。
 */
export function Sidebar({
  actions,
  onOpenSettings,
  onClose,
}: {
  actions: AppAction[]
  onOpenSettings: (tab: SettingsTab) => void
  onClose?: () => void
}) {
  const { connected, connecting, newChat } = useAgentSession()
  const { settings } = useSettings()
  const { t } = useI18n()

  const status = connecting ? 'connecting' : connected ? 'connected' : 'disconnected'
  const statusDot =
    status === 'connected'
      ? 'bg-success'
      : status === 'connecting'
        ? 'bg-warning animate-pulse'
        : 'bg-muted'
  const statusLabel =
    status === 'connected'
      ? t('sidebar.connected')
      : status === 'connecting'
        ? t('sidebar.connecting')
        : t('sidebar.disconnected')
  // 安全解析主机名：serverUrl 可能是用户手输的任意字符串，渲染期抛异常会白屏。
  let serverHost = '—'
  try {
    serverHost = settings.serverUrl ? new URL(settings.serverUrl).host : '—'
  } catch {
    /* 非法地址保持占位符，交由设置面板校验与错误横幅提示 */
  }

  const navActions = NAV_IDS.map((id) => actions.find((a) => a.id === id)).filter(
    (a): a is AppAction => !!a,
  )

  return (
    <div className="glass-bar flex h-full w-64 flex-col border-r">
      {/* 品牌 —— 行高与顶栏一致（h-14），让外壳上沿只有一条水平基准线 */}
      <div className="flex h-14 shrink-0 items-center gap-2.5 px-4">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gradient-to-br from-primary to-primary-glow text-primary-fg shadow-glow">
          <Icon name="command" size={16} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="truncate font-display text-md font-bold leading-none tracking-tight text-text">
            Agent<span className="text-primary"> ·</span> Console
          </div>
          <div className="brand-sub mt-1 truncate">
            {t('sidebar.brand')}
          </div>
        </div>
        {onClose && (
          <IconButton
            icon="close"
            label={t('common.close')}
            size="sm"
            onClick={onClose}
            className="-mr-1 text-muted lg:hidden"
          />
        )}
      </div>

      {/* 主行动：新建对话（唯一常驻的高饱和按钮） */}
      <div className="shrink-0 px-3 pb-2">
        <Button
          variant="primary"
          className="w-full"
          leftIcon="plus"
          onClick={() => {
            newChat()
            onClose?.()
          }}
        >
          {t('sidebar.new_chat')}
        </Button>
      </div>

      {/* 会话历史（弹性区域） */}
      <SessionList onClose={onClose} />

      {/* 全局入口区：视图导航 / 设置 / 连接状态 / 版本 */}
      <div className="shrink-0 border-t border-border px-2.5 py-2">
        <nav aria-label={t('sidebar.nav_views')} className="space-y-0.5">
          {navActions.map((a) => (
            <NavRow
              key={a.id}
              icon={a.icon}
              label={a.label}
              onClick={() => {
                a.run()
                onClose?.()
              }}
            />
          ))}
        </nav>

        <Divider className="my-2" />

        <NavRow
          icon="settings"
          label={t('sidebar.settings')}
          onClick={() => {
            onOpenSettings('connection')
            onClose?.()
          }}
        />

        <Divider className="my-2" />

        {/* 连接状态：可点击 → 设置·连接（把只读信息变成有去处的控件） */}
        <button
          type="button"
          onClick={() => {
            onOpenSettings('connection')
            onClose?.()
          }}
          title={t('sidebar.connection_hint')}
          className="focus-ring group flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left transition-colors hover:bg-surface-2"
        >
          <span className={cn('h-1.5 w-1.5 shrink-0 rounded-full', statusDot)} />
          <span className="min-w-0 flex-1">
            <span className="block truncate text-xs font-medium text-text-2">{statusLabel}</span>
            <span className="block truncate font-mono text-2xs text-muted">{serverHost}</span>
          </span>
          <Icon
            name="chevron-right"
            size={14}
            className="shrink-0 text-muted opacity-0 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
          />
        </button>

        <div className="mt-1 flex items-center justify-between px-2.5 text-2xs text-muted">
          <span className="tabular">v{__APP_VERSION__}</span>
          <span className="inline-flex items-center gap-1">
            <Icon name="github" size={12} />
            {t('sidebar.source')}
          </span>
        </div>
      </div>
    </div>
  )
}

/** 侧栏导航行（图标 + 标签），全站唯一的导航行样式。 */
function NavRow({
  icon,
  label,
  onClick,
  active,
}: {
  icon: string
  label: string
  onClick: () => void
  active?: boolean
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        'focus-ring flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-xs font-medium transition-colors',
        active ? 'bg-primary/10 text-primary' : 'text-text-2 hover:bg-surface-2 hover:text-text',
      )}
    >
      <Icon name={icon} size={16} className={cn('shrink-0', active ? 'text-primary' : 'text-muted')} />
      <span className="min-w-0 flex-1 truncate text-left">{label}</span>
    </button>
  )
}
