import { useCallback, useEffect, useMemo, useState } from 'react'
import type { BranchNode, BranchTree } from '@/lib/agent/types'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useNotifications } from '@/lib/notifications'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/cn'
import { treeIndent } from '@/lib/tree'
import { Icon } from '@/components/icons'
import { Button, Modal, Skeleton } from '@/components/ui'

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
  const [error, setError] = useState<string | null>(null)
  const [handoff, setHandoff] = useState(false)
  const [busyLeaf, setBusyLeaf] = useState<string | null>(null)

  // 拉取分支树（可重试）：错误与空树分开展示，失败不再呈现空白盒。
  const load = useCallback(async () => {
    if (!sessionId) return
    setLoading(true)
    setError(null)
    setTree(null)
    const res = await fetchBranches(sessionId)
    setTree(res.tree)
    setError(res.error)
    setLoading(false)
  }, [sessionId, fetchBranches])

  // 打开时拉取分支树。
  useEffect(() => {
    if (!open) return
    void load()
  }, [open, load])

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
    // 显式传目标会话：模态框浏览的是 sessionId 的分支树，切换必须作用于同一会话，
    // 否则会把续写点错落到当前活跃会话上。
    const res = await switchBranch(leafId, handoff, sessionId)
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
        <label className="flex shrink-0 cursor-pointer select-none items-center gap-1.5 text-2xs text-text-2">
          <input
            type="checkbox"
            checked={handoff}
            onChange={(e) => setHandoff(e.target.checked)}
            className="h-3.5 w-3.5 accent-primary"
          />
          {t('branches.handoff')}
        </label>
      </div>

      <div className="card-inset no-scrollbar max-h-[55vh] overflow-y-auto p-2">
        {loading && (
          <div className="space-y-2 px-1 py-2">
            {Array.from({ length: 4 }).map((_, i) => (
              <Skeleton key={i} className={cn('h-5 rounded', ['w-3/4', 'w-2/3', 'w-4/5', 'w-3/5'][i])} />
            ))}
          </div>
        )}

        {!loading && error && (
          <div className="flex flex-col items-center gap-2 px-3 py-8 text-center">
            <Icon name="alert" size={20} className="text-danger" />
            <p className="text-xs font-medium text-text-2">{t('branches.load_failed')}</p>
            {error && (
              <p className="max-w-xs break-all text-2xs leading-relaxed text-muted">{error}</p>
            )}
            <Button size="sm" variant="outline" leftIcon="refresh" onClick={() => void load()}>
              {t('common.retry')}
            </Button>
          </div>
        )}

        {!loading && !error && tree && tree.nodes.length === 0 && (
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
        <p className="mt-2 text-2xs text-muted">{t('branches.single_hint')}</p>
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
  t: (key: string, args?: Record<string, unknown>) => string
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
        style={{ paddingLeft: `${treeIndent(depth)}px` }}
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
          size={12}
          className={cn('shrink-0', onActivePath ? 'text-primary/80' : 'text-muted')}
        />
        <span className="min-w-0 flex-1">
          <span
            className={cn(
              'block truncate text-2xs leading-tight',
              isActive ? 'font-medium text-primary' : 'text-text-2',
            )}
            title={node.preview}
          >
            {node.preview || t('branches.no_preview')}
          </span>
          <span className="section-label">
            {roleLabel(node.role, t)}
            {isLeaf && kids.length === 0 ? '' : ''}
          </span>
        </span>
        {/* 叶子且非当前活跃：可切换。 */}
        {isLeaf && !isActive && (
          <button
            disabled={busyLeaf !== null}
            onClick={() => onSwitch(node.id)}
            className={cn(
              'shrink-0 rounded px-1.5 py-0.5 text-2xs font-medium transition-colors',
              'bg-surface-3 text-text-2 hover:bg-primary hover:text-primary-fg',
              'disabled:cursor-not-allowed disabled:opacity-50',
            )}
          >
            {busyLeaf === node.id ? '…' : t('branches.switch')}
          </button>
        )}
        {isActive && (
          <span className="shrink-0 rounded bg-primary/20 px-1.5 py-0.5 text-2xs font-medium text-primary">
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

/** 角色的人类可读标签（t 由组件注入）。 */
function roleLabel(
  role: string,
  t: (key: string, args?: Record<string, unknown>) => string,
): string {
  switch (role) {
    case 'user':
      return t('branches.role.user')
    case 'assistant':
      return t('branches.role.assistant')
    case 'tool':
      return t('branches.role.tool')
    case 'status':
      return t('branches.role.status')
    case 'ask':
      return t('branches.role.ask')
    default:
      return role
  }
}

