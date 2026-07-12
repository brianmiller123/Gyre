import { useState } from 'react'
import { Button, Divider } from '@/components/ui'
import { Icon } from '@/components/icons'
import { cn } from '@/lib/cn'
import { SessionList } from '@/components/agent/SessionList'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useSettings } from '@/lib/settings'
import { useTheme } from '@/lib/theme'
import { useI18n } from '@/lib/i18n'
import { SUPPORTED_LOCALES } from '@/lib/locales'
import type { LocaleCode } from '@/lib/locales'

/**
 * 左侧控制栏：品牌、新建对话、会话管理列表（主区域）、连接状态、操作与外观。
 *
 * 历史会话的展示与交互（搜索 / 切换 / 重命名 / 删除 / 加载与空态等）已下沉到
 * [`SessionList`]，本组件仅保留全局骨架（品牌、连接卡片、设置入口、主题与语言）。
 */
export function Sidebar({
  onOpenSettings,
  onOpenWorkspace,
  onClose,
}: {
  onOpenSettings: () => void
  onOpenWorkspace: () => void
  onClose?: () => void
}) {
  const { connected, connecting, newChat, clear, error, sessionId } = useAgentSession()
  const { settings } = useSettings()
  const { theme, toggle } = useTheme()
  const { t, preference, setPreference, locale } = useI18n()
  const [langMenuOpen, setLangMenuOpen] = useState(false)

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

  return (
    <div className="flex h-full w-60 flex-col border-r border-border bg-surface/80 backdrop-blur-xl">
      {/* 品牌 */}
      <div className="flex h-16 shrink-0 items-center gap-2.5 px-4">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-gradient-to-br from-primary to-primary-glow text-white shadow-glow">
          <Icon name="command" size={18} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="font-display text-[15px] font-bold leading-none tracking-tight text-text">
            Agent<span className="text-primary"> ·</span> Console
          </div>
          <div className="mt-1 text-[10px] uppercase tracking-[0.16em] text-muted">
            {t('sidebar.brand')}
          </div>
        </div>
        {onClose && (
          <button
            onClick={onClose}
            className="flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-surface-2 hover:text-text lg:hidden"
          >
            <Icon name="close" size={18} />
          </button>
        )}
      </div>

      {/* 新建对话 */}
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

      {/* 会话管理列表（主区域，可滚动） */}
      <SessionList onClose={onClose} />

      {/* 连接卡片 */}
      <div className="mx-3 mt-2 shrink-0 rounded-xl border border-border bg-surface-2/60 p-3">
        <div className="flex items-center justify-between">
          <span className="flex items-center gap-2 text-xs font-medium text-text-2">
            <span className={`h-2 w-2 rounded-full ${statusDot}`} />
            {statusLabel}
          </span>
        </div>
        <p className="mt-1.5 truncate font-mono text-[10px] text-muted">
          {settings.serverUrl ? new URL(settings.serverUrl).host : '—'}
        </p>
        {sessionId && (
          <p className="mt-0.5 truncate font-mono text-[10px] text-muted">
            session: {sessionId.slice(0, 13)}…
          </p>
        )}
        {error && <p className="mt-1 text-[10px] text-danger">{error}</p>}
      </div>

      {/* 操作（浏览 / 设置 / 清空） */}
      <nav className="mt-2 grid shrink-0 grid-cols-3 gap-1 px-3">
        <NavAction icon="layers" label={t('sidebar.browse')} onClick={() => { onOpenWorkspace(); onClose?.() }} />
        <NavAction icon="settings" label={t('sidebar.settings')} onClick={() => { onOpenSettings(); onClose?.() }} />
        <NavAction icon="trash" label={t('sidebar.clear')} danger onClick={() => { clear(); onClose?.() }} />
      </nav>

      {/* 外观：主题 / 语言 */}
      <div className="shrink-0 px-3 pb-3 pt-2">
        <Divider className="mb-2" />
        <button
          onClick={toggle}
          className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm text-text-2 transition-colors hover:bg-surface-2 hover:text-text"
        >
          <Icon name={theme === 'dark' ? 'sun' : 'moon'} size={17} />
          {theme === 'dark' ? t('sidebar.light') : t('sidebar.dark')}
        </button>
        {/* 语言切换快捷按钮 */}
        <div className="relative">
          <button
            onClick={() => setLangMenuOpen(!langMenuOpen)}
            className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm text-text-2 transition-colors hover:bg-surface-2 hover:text-text"
          >
            <Icon name="globe" size={17} />
            <span className="flex-1 text-left">{t(`lang.${preference === 'auto' ? locale : preference}`)}</span>
            <Icon name="chevron-down" size={13} className="text-muted" />
          </button>
          {langMenuOpen && (
            <>
              <div className="absolute bottom-full left-0 right-0 z-50 mb-1 overflow-hidden rounded-lg border border-border bg-surface shadow-lg">
                <div className="py-1">
                  <button
                    onClick={() => { setPreference('auto'); setLangMenuOpen(false) }}
                    className={cn(
                      'flex w-full items-center gap-2.5 px-3 py-1.5 text-xs transition-colors',
                      preference === 'auto' ? 'bg-primary/10 text-primary' : 'text-text-2 hover:bg-surface-2 hover:text-text',
                    )}
                  >
                    {preference === 'auto' && <Icon name="check" size={12} className="shrink-0" />}
                    <span className={cn(preference !== 'auto' && 'pl-[20px]')}>{t('lang.auto')}</span>
                  </button>
                  {SUPPORTED_LOCALES.map((code: LocaleCode) => (
                    <button
                      key={code}
                      onClick={() => { setPreference(code); setLangMenuOpen(false) }}
                      className={cn(
                        'flex w-full items-center gap-2.5 px-3 py-1.5 text-xs transition-colors',
                        preference === code ? 'bg-primary/10 text-primary' : 'text-text-2 hover:bg-surface-2 hover:text-text',
                      )}
                    >
                      {preference === code && <Icon name="check" size={12} className="shrink-0" />}
                      <span className={cn(preference !== code && 'pl-[20px]')}>{t(`lang.${code}`)}</span>
                    </button>
                  ))}
                </div>
              </div>
              <div className="fixed inset-0 z-40" onClick={() => setLangMenuOpen(false)} />
            </>
          )}
        </div>
        <div className="mt-1 flex items-center justify-between px-2.5 py-1 text-[10px] text-muted">
          <span>v1.0 · WebUI</span>
          <a
            href="https://github.com"
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 hover:text-text"
          >
            <Icon name="github" size={12} /> {t('sidebar.source')}
          </a>
        </div>
      </div>
    </div>
  )
}

/** 紧凑操作按钮（图标 + 标签，用于底部 3 列网格）。 */
function NavAction({
  icon,
  label,
  onClick,
  danger,
}: {
  icon: string
  label: string
  onClick: () => void
  danger?: boolean
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      className={`flex flex-col items-center gap-1 rounded-lg px-1 py-2 text-[10px] font-medium transition-colors hover:bg-surface-2 ${
        danger ? 'text-danger hover:bg-danger/10' : 'text-text-2 hover:text-text'
      }`}
    >
      <Icon name={icon} size={17} />
      <span className="max-w-full truncate">{label}</span>
    </button>
  )
}
