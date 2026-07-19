import { useEffect, useMemo, useState } from 'react'
import type { BranchNode, BranchTree } from '@/lib/agent/types'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useNotifications } from '@/lib/notifications'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/cn'
import { Icon } from '@/components/icons'
import { Modal, Skeleton } from '@/components/ui'

/**
 * 会话分支树模态框：渲染某会话的节点森林（缩进树形），高亮活跃路径，
 * 支持把续写点切换到任意叶子（可选注入被离开分支的 handoff 摘要）。
 *
 * 数据来自 `GET /api/sessions/{id}/branches`；切换走 `POST .../branches/switch`，
 * 成功后由 useAgentSession.switchBranch 自动重连 resume 重载新分支的 transcript。
 */
export function BranchTreeModal({
  sessionId,
  open,
  onClose,
}: {
  sessionId: string
  open: boolean
  onClose: () => void
}) {
  const { fetchBranches, switchBranch } = useAgentSession()
  const { toast } = useNotifications()
  const { t } = useI18n()

  const [tree, setTree] = useState<BranchTree | null>(null)
  const [loading, setLoading] = useState(false)
  const [handoff, setHandoff] = useState(false)
  const [busyLeaf, setBusyLeaf] = useState<string | null>(null)

  // 打开时拉取分支树。
  useEffect(() => {
    if (!open || !sessionId) return
    let cancelled = false
    setLoading(true)
    setTree(null)
    void (async () => {
      const data = await fetchBranches(sessionId)
      if (!cancelled) {
        setTree(data)
        setLoading(false)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [open, sessionId, fetchBranches])

  // 活跃路径节点 id 集（active_leaf → root），用于高亮当前分支。
  const activePathIds = useMemo(() => {
    const map = new Map<string, BranchNode>()
    for (const n of tree?.nodes ?? []) map.set(n.id, n)
    const set = new Set<string>()
    let cur = tree?.active_leaf ?? null
    while (cur) {
      const node = map.get(cur)
      if (!node) break
      if (set.has(cur)) break // 环路防御
      set.add(cur)
      cur = node.parent_id
    }
    return set
  }, [tree])

  // 按 parent_id 建子节点表（保持 nodes 顺序）。
  const childrenOf = useMemo(() => {
    const m = new Map<string | null, BranchNode[]>()
    for (const n of tree?.nodes ?? []) {
      const key = n.parent_id
      const arr = m.get(key) ?? []
      arr.push(n)
      m.set(key, arr)
    }
    return m
  }, [tree])

  const roots = childrenOf.get(null) ?? []

  const onSwitch = async (leafId: string) => {
    setBusyLeaf(leafId)
    const res = await switchBranch(leafId, handoff)
    setBusyLeaf(null)
    if (res.ok) {
      toast({ title: t('branches.switched'), severity: 'success' })
      onClose()
    } else {
      toast({ title: t('branches.switch_failed'), body: res.error, severity: 'danger' })
    }
  }

  const leafSet = useMemo(() => new Set(tree?.leaves ?? []), [tree])
  const multiBranch = (tree?.leaves.length ?? 0) > 1

  return (
    <Modal open={open} onClose={onClose} title={t('branches.title')} size="md">
      <div className="mb-2 flex items-center justify-between gap-2">
        <p className="text-xs text-muted">{t('branches.desc')}</p>
        <label className="flex shrink-0 cursor-pointer select-none items-center gap-1.5 text-[11px] text-text-2">
          <input
            type="checkbox"
            checked={handoff}
            onChange={(e) => setHandoff(e.target.checked)}
            className="h-3.5 w-3.5 accent-[var(--primary)]"
          />
          {t('branches.handoff')}
        </label>
      </div>

      <div className="no-scrollbar max-h-[55vh] overflow-y-auto rounded-lg border border-border bg-surface p-2">
        {loading && (
          <div className="space-y-2 px-1 py-2">
            {Array.from({ length: 4 }).map((_, i) => (
              <Skeleton key={i} className={cn('h-5 rounded', ['w-3/4', 'w-2/3', 'w-4/5', 'w-3/5'][i])} />
            ))}
          </div>
        )}

        {!loading && tree && tree.nodes.length === 0 && (
          <div className="flex items-center justify-center px-3 py-8 text-xs text-muted">
            {t('branches.empty')}
          </div>
        )}

        {!loading && tree && tree.nodes.length > 0 && (
          <ul className="space-y-0.5">
            {roots.map((node) => (
              <BranchNodeView
                key={node.id}
                node={node}
                depth={0}
                childrenOf={childrenOf}
                activeId={tree.active_leaf}
                activePathIds={activePathIds}
                leafSet={leafSet}
                busyLeaf={busyLeaf}
                onSwitch={onSwitch}
                t={t}
              />
            ))}
          </ul>
        )}
      </div>

      {tree && !multiBranch && !loading && (
        <p className="mt-2 text-[11px] text-muted">{t('branches.single_hint')}</p>
      )}
    </Modal>
  )
}

/* ---------------------------- 单节点递归渲染 ----------------------------- */

function BranchNodeView({
  node,
  depth,
  childrenOf,
  activeId,
  activePathIds,
  leafSet,
  busyLeaf,
  onSwitch,
  t,
}: {
  node: BranchNode
  depth: number
  childrenOf: Map<string | null, BranchNode[]>
  activeId: string | null
  activePathIds: Set<string>
  leafSet: Set<string>
  busyLeaf: string | null
  onSwitch: (leafId: string) => void
  t: (key: string, args?: Record<string, string | number>) => string
}) {
  const kids = childrenOf.get(node.id) ?? []
  const isActive = node.id === activeId
  const onActivePath = activePathIds.has(node.id)
  const isLeaf = leafSet.has(node.id)

  return (
    <li>
      <div
        className={cn(
          'group flex items-center gap-1.5 rounded-md px-1.5 py-1 transition-colors',
          isActive ? 'bg-primary/15' : 'hover:bg-surface-2',
        )}
        style={{ paddingLeft: `${depth * 14 + 6}px` }}
      >
        {/* 分支节点圆点：活跃叶子为实心主色，路径上为半透明，其余淡灰。 */}
        <span
          className={cn(
            'h-1.5 w-1.5 shrink-0 rounded-full',
            isActive
              ? 'bg-primary'
              : onActivePath
                ? 'bg-primary/50'
                : 'bg-muted/40',
          )}
        />
        <Icon
          name="message-square"
          size={11}
          className={cn('shrink-0', onActivePath ? 'text-primary/80' : 'text-muted')}
        />
        <span className="min-w-0 flex-1">
          <span
            className={cn(
              'block truncate text-[11.5px] leading-tight',
              isActive ? 'font-medium text-primary' : 'text-text-2',
            )}
            title={node.preview}
          >
            {node.preview || t('branches.no_preview')}
          </span>
          <span className="text-[9px] uppercase tracking-wide text-muted/70">
            {roleLabel(node.role)}
            {isLeaf && kids.length === 0 ? '' : ''}
          </span>
        </span>
        {/* 叶子且非当前活跃：可切换。 */}
        {isLeaf && !isActive && (
          <button
            disabled={busyLeaf !== null}
            onClick={() => onSwitch(node.id)}
            className={cn(
              'shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium transition-colors',
              'bg-surface-3 text-text-2 hover:bg-primary hover:text-white',
              'disabled:cursor-not-allowed disabled:opacity-50',
            )}
          >
            {busyLeaf === node.id ? '…' : t('branches.switch')}
          </button>
        )}
        {isActive && (
          <span className="shrink-0 rounded bg-primary/20 px-1.5 py-0.5 text-[9.5px] font-medium text-primary">
            {t('branches.current')}
          </span>
        )}
      </div>
      {kids.length > 0 && (
        <ul className="space-y-0.5">
          {kids.map((kid) => (
            <BranchNodeView
              key={kid.id}
              node={kid}
              depth={depth + 1}
              childrenOf={childrenOf}
              activeId={activeId}
              activePathIds={activePathIds}
              leafSet={leafSet}
              busyLeaf={busyLeaf}
              onSwitch={onSwitch}
              t={t}
            />
          ))}
        </ul>
      )}
    </li>
  )
}

/** 角色的人类可读标签。 */
function roleLabel(role: string): string {
  switch (role) {
    case 'user':
      return '用户'
    case 'assistant':
      return '助手'
    case 'tool':
      return '工具'
    case 'status':
      return '状态'
    case 'ask':
      return '询问'
    default:
      return role
  }
}

