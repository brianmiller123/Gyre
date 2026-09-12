import { useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { Icon } from '@/components/icons'
import { Kbd, useDialogA11y } from '@/components/ui'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'

/**
 * 全局动作（App Action）——界面里每一个**可达功能**的唯一描述。
 *
 * 信息架构的核心约定：任何功能都只能在 `AppAction[]` 里登记一次，然后由
 * 三个渲染器消费同一份数据：
 *   1. `CommandPalette`（⌘K，跨类目检索）
 *   2. 侧边栏导航 / 顶栏溢出菜单（按 `group` 过滤后的常驻入口）
 *   3. 输入框的斜杠命令（复用同一批 `ctx` 方法）
 *
 * 这样"某功能在 A 处有、B 处没有"或"同一动作两套文案/图标"在结构上不可能
 * 发生——历史版本里「清空对话」散落在侧栏、顶栏溢出菜单与设置面板三处，并各
 * 自带一个确认弹窗，正是缺少这层单一事实源所致。
 */
export interface AppAction {
  /** 稳定标识（同时作为 i18n key 的一部分与 React key）。 */
  id: string
  /** 已本地化的标题。 */
  label: string
  /** 已本地化的副标题（检索命中项时展示的一句话解释）。 */
  hint?: string
  icon: string
  /** 命令面板内的分组；也是侧栏/顶栏筛选的维度。 */
  group: 'session' | 'view' | 'preference'
  /** 展示用的快捷键提示（真正的事件绑定在 AgentShell 的全局 keydown 里）。 */
  shortcut?: string
  /** 额外检索词（英文/拼音/旧名），提升命中率。 */
  keywords?: string
  danger?: boolean
  disabled?: boolean
  run: () => void
}

const GROUP_ORDER: AppAction['group'][] = ['session', 'view', 'preference']

/**
 * 模糊匹配：对标题与关键词做「子序列 + 前缀加权」评分。
 *
 * 只做客户端本地过滤（动作总数 ~12），因此不需要索引或 worker；评分只用于
 * 排序，保证「统计」既能被 "tj" 也能被 "stats" 命中。
 */
function score(action: AppAction, q: string): number {
  if (!q) return 0
  const hay = `${action.label} ${action.keywords ?? ''}`.toLowerCase()
  const needle = q.toLowerCase().trim()
  if (!needle) return 0
  if (hay.startsWith(needle)) return 3
  if (hay.includes(needle)) return 2
  // 子序列匹配（"tj" → "统计" 的拼音首字母 / "sc" → "statistics"）
  let i = 0
  for (const ch of hay) {
    if (ch === needle[i]) i++
    if (i === needle.length) return 1
  }
  return -1
}

/**
 * ⌘K / Ctrl+K 命令面板：跨类目检索并执行任意已登记动作。
 *
 * 交互依据：当功能入口天然分散在「会话生命周期 / 视图 / 偏好」三个维度时，
 * 让它们**都留在各自语义正确的位置**、再补一个统一的检索入口，比把所有按钮
 * 硬塞进一条工具栏更符合用户心智——熟练用户走快捷键，新用户走可见导航。
 */
export function CommandPalette({
  open,
  onClose,
  actions,
}: {
  open: boolean
  onClose: () => void
  actions: AppAction[]
}) {
  const { t } = useI18n()
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLUListElement>(null)
  const a11y = useDialogA11y(open, onClose)

  // 打开即重置查询并聚焦输入框（hook 先把焦点收到容器，这里覆盖到输入框）。
  useEffect(() => {
    if (!open) return
    setQuery('')
    setActive(0)
    const id = requestAnimationFrame(() => inputRef.current?.focus())
    return () => cancelAnimationFrame(id)
  }, [open])

  const results = useMemo(() => {
    const q = query.trim()
    const scored = actions
      .map((a) => ({ a, s: score(a, q) }))
      .filter((x) => x.s >= 0)
      .sort((x, y) => y.s - x.s)
    return scored.map((x) => x.a)
  }, [actions, query])

  // 键盘高亮项跟随滚动，避免长列表中选中项滚出可视区。
  useEffect(() => {
    listRef.current?.children[active]?.scrollIntoView({ block: 'nearest' })
  }, [active, results])

  const run = (a: AppAction) => {
    if (a.disabled) return
    onClose()
    // 先关闭面板再执行：动作可能卸载本组件（如打开全屏面板），
    // 顺序反过来会触发对已卸载组件的 setState。
    a.run()
  }

  if (!open) return null

  const groups = GROUP_ORDER.map((g) => ({ g, items: results.filter((a) => a.group === g) })).filter(
    (x) => x.items.length > 0,
  )

  return createPortal(
    <div className="fixed inset-0 z-palette flex items-start justify-center p-4 pt-[12vh]">
      <div className="app-backdrop" onClick={onClose} />
      <div
        ref={a11y.ref}
        onKeyDown={a11y.onKeyDown}
        role="dialog"
        aria-modal="true"
        aria-label={t('palette.title')}
        tabIndex={-1}
        className="overlay-panel max-h-[min(70vh,560px)] w-full max-w-xl animate-palette-in rounded-xl"
      >
        {/* 检索行 */}
        <div className="flex items-center gap-2.5 border-b border-border px-4 py-3">
          <Icon name="search" size={16} className="shrink-0 text-muted" />
          <input
            ref={inputRef}
            value={query}
            onChange={(e) => {
              setQuery(e.target.value)
              setActive(0)
            }}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing || e.keyCode === 229) return
              if (e.key === 'ArrowDown') {
                e.preventDefault()
                setActive((i) => (results.length ? (i + 1) % results.length : 0))
              } else if (e.key === 'ArrowUp') {
                e.preventDefault()
                setActive((i) => (results.length ? (i - 1 + results.length) % results.length : 0))
              } else if (e.key === 'Enter') {
                e.preventDefault()
                const a = results[active]
                if (a) run(a)
              }
            }}
            placeholder={t('palette.placeholder')}
            aria-label={t('palette.placeholder')}
            aria-controls="palette-list"
            aria-activedescendant={results[active] ? `palette-opt-${results[active].id}` : undefined}
            role="combobox"
            aria-expanded
            aria-autocomplete="list"
            className="w-full bg-transparent text-base text-text placeholder:text-muted focus:outline-none"
          />
          <Kbd className="hidden shrink-0 sm:inline-flex">esc</Kbd>
        </div>

        {/* 结果 */}
        <ul
          ref={listRef}
          id="palette-list"
          role="listbox"
          aria-label={t('palette.title')}
          className="min-h-0 flex-1 overflow-y-auto p-1.5"
        >
          {results.length === 0 && (
            <li className="px-3 py-10 text-center text-sm text-muted">{t('palette.empty')}</li>
          )}
          {groups.map(({ g, items }) => (
            <li key={g}>
              <p className="section-label px-2.5 pb-1 pt-2">{t(`palette.group.${g}`)}</p>
              <ul>
                {items.map((a) => {
                  const idx = results.indexOf(a)
                  return (
                    <li
                      key={a.id}
                      id={`palette-opt-${a.id}`}
                      role="option"
                      aria-selected={idx === active}
                    >
                      <button
                        type="button"
                        disabled={a.disabled}
                        onMouseEnter={() => setActive(idx)}
                        onClick={() => run(a)}
                        className={cn(
                          'flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-40',
                          idx === active ? 'bg-surface-2 text-text' : 'text-text-2',
                          a.danger && 'text-danger',
                        )}
                      >
                        <Icon
                          name={a.icon}
                          size={16}
                          className={cn('shrink-0', a.danger ? 'text-danger' : idx === active ? 'text-primary' : 'text-muted')}
                        />
                        <span className="min-w-0 flex-1 truncate font-medium">{a.label}</span>
                        {a.hint && (
                          <span className="hidden max-w-[45%] truncate text-2xs text-muted sm:block">
                            {a.hint}
                          </span>
                        )}
                        {a.shortcut && <Kbd className="shrink-0">{a.shortcut}</Kbd>}
                      </button>
                    </li>
                  )
                })}
              </ul>
            </li>
          ))}
        </ul>

        {/* 底部提示 */}
        <div className="flex shrink-0 items-center gap-3 border-t border-border px-4 py-2 text-2xs text-muted">
          <span className="flex items-center gap-1">
            <Kbd>↑</Kbd>
            <Kbd>↓</Kbd> {t('palette.hint_move')}
          </span>
          <span className="flex items-center gap-1">
            <Kbd>↵</Kbd> {t('palette.hint_run')}
          </span>
          <span className="ml-auto hidden sm:block">{t('palette.count', { n: results.length })}</span>
        </div>
      </div>
    </div>,
    document.body,
  )
}
