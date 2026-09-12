import { useCallback, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import {
  fetchFile,
  fetchListing,
  fetchWorkspace,
  languageOf,
  type FsEntry,
  type FileContent,
  type WorkspaceInfo,
} from '@/lib/agent/workspace'
import { useHighlightedCode, useHighlighter } from '@/lib/agent/highlight'
import { useTheme } from '@/lib/theme'
import { Icon } from '@/components/icons'
import { Badge, Button, IconButton, Spinner } from '@/components/ui'
import { compact } from '@/lib/format'
import { cn } from '@/lib/cn'
import { treeIndent } from '@/lib/tree'
import { useI18n } from '@/lib/i18n'
import { useNotifications } from '@/lib/notifications'

type WsMode = 'left' | 'floating' | 'right'

interface TreeNode {
  name: string
  path: string
  kind: 'dir' | 'file'
  size: number
  loaded?: boolean
  children?: TreeNode[]
  loading?: boolean
}

const STORAGE = 'agent-ws-window'
const MIN_W = 420
const MIN_H = 300
const MIN_DOCK = 280
const MAX_DOCK = 720
/** 浮动窗口与视口边缘的安全间距——所有夹取计算的唯一边距来源。 */
const EDGE = 8
/** 文件树栏宽的可调范围；VIEWER_MIN 保证代码预览区不会被挤成 0 宽。 */
const TREE_MIN = 140
const TREE_MAX = 480
const VIEWER_MIN = 200

interface SavedWin {
  mode: WsMode
  pos?: { x: number; y: number }
  size?: { w: number; h: number }
  dockW?: number
  treeW?: number
}

function loadSaved(): SavedWin {
  try {
    const raw = localStorage.getItem(STORAGE)
    if (raw) return JSON.parse(raw)
  } catch {
    /* ignore */
  }
  return { mode: 'floating' }
}

function clamp(n: number, min: number, max: number) {
  return Math.min(max, Math.max(min, n))
}

/**
 * 把浮动窗口整体夹回视口内，返回校正后的 `{x, y, w, h}`。
 *
 * 必须**先定尺寸、再用尺寸定位置**：分开独立夹取会让右下角的缩放把手落到
 * 视口之外（旧实现里 size 用 `innerWidth-32`、pos 用 `innerWidth-160`，底边
 * 最远可达 `2×innerHeight-200`）。视口比 MIN_W/MIN_H 还小时以视口为准。
 */
function fitFloating(
  pos: { x: number; y: number },
  size: { w: number; h: number },
  vw: number,
  vh: number,
) {
  const w = clamp(size.w, MIN_W, vw - EDGE * 2)
  const h = clamp(size.h, MIN_H, vh - EDGE * 2)
  const x = clamp(pos.x, EDGE, vw - w - EDGE)
  const y = clamp(pos.y, EDGE, vh - h - EDGE)
  return { x, y, w, h }
}

/**
 * File browser rendered as a dockable, draggable and resizable window.
 *
 *  - `left` / `right`: full-height side rail whose width is adjustable by
 *    dragging its inner edge.
 *  - `floating`: free-floating window — drag the title bar to move, drag the
 *    bottom-right grip to resize.
 *
 * The file tree / code-viewer split is itself adjustable via a draggable
 * divider. All geometry + the chosen mode persist to localStorage.
 */
export function WorkspacePanel({ onClose }: { onClose?: () => void }) {
  const saved = useRef<SavedWin>(loadSaved())
  const vw = typeof window !== 'undefined' ? window.innerWidth : 1280
  const vh = typeof window !== 'undefined' ? window.innerHeight : 800

  const [mode, setMode] = useState<WsMode>(saved.current.mode)
  // 保存的几何可能来自更大的屏幕：首帧就按当前视口夹取，否则右下角缩放把手会
  // 落在视口外，面板既不能缩放也拖不回可见区域（旧 bug）。
  const seed = useRef(
    fitFloating(
      saved.current.pos ?? { x: (vw - 760) / 2, y: 64 },
      saved.current.size ?? { w: 760, h: 640 },
      vw,
      vh,
    ),
  )
  const [pos, setPos] = useState<{ x: number; y: number }>({ x: seed.current.x, y: seed.current.y })
  const [size, setSize] = useState<{ w: number; h: number }>({ w: seed.current.w, h: seed.current.h })
  const [dockW, setDockW] = useState<number>(saved.current.dockW ?? 360)
  const [treeW, setTreeW] = useState<number>(saved.current.treeW ?? 208)

  // 指针拖动回调保持 [] 依赖（拖拽中途重建 handler 会打断 pointer capture），
  // 所以用 ref 读取最新几何，而不是闭包变量。
  const geo = useRef({ pos, size })
  geo.current = { pos, size }

  // Persist all geometry.
  useEffect(() => {
    try {
      localStorage.setItem(
        STORAGE,
        JSON.stringify({
          mode,
          pos: mode === 'floating' ? pos : undefined,
          size: mode === 'floating' ? size : undefined,
          dockW,
          treeW,
        }),
      )
    } catch {
      /* ignore */
    }
  }, [mode, pos, size, dockW, treeW])

  // 视口变化时把浮动窗口整体夹回可见区域；进入 floating（含 dock→floating）
  // 时立即执行一次——旧实现只在 resize 事件里夹取，启动/切模式时从不夹取。
  useEffect(() => {
    if (mode !== 'floating') return
    const apply = () => {
      const f = fitFloating(geo.current.pos, geo.current.size, window.innerWidth, window.innerHeight)
      setSize((s) => (s.w === f.w && s.h === f.h ? s : { w: f.w, h: f.h }))
      setPos((p) => (p.x === f.x && p.y === f.y ? p : { x: f.x, y: f.y }))
    }
    apply()
    window.addEventListener('resize', apply)
    return () => window.removeEventListener('resize', apply)
  }, [mode])

  // --- move (floating) ---
  const move = useRef<{ sx: number; sy: number; px: number; py: number } | null>(null)
  const onMoveStart = useCallback(
    (e: React.PointerEvent) => {
      if (mode !== 'floating') return
      if ((e.target as HTMLElement).closest('[data-no-drag]')) return
      move.current = { sx: e.clientX, sy: e.clientY, px: pos.x, py: pos.y }
      ;(e.target as HTMLElement).setPointerCapture?.(e.pointerId)
    },
    [mode, pos],
  )
  const onMoveDrag = useCallback((e: React.PointerEvent) => {
    if (!move.current) return
    const dx = e.clientX - move.current.sx
    const dy = e.clientY - move.current.sy
    // 夹取必须减去面板自身尺寸，否则 760px 宽的面板能拖到 x=innerWidth-120，
    // 右边缘连同缩放把手直接越出视口。
    const { w, h } = geo.current.size
    setPos({
      x: clamp(move.current.px + dx, EDGE, window.innerWidth - w - EDGE),
      y: clamp(move.current.py + dy, EDGE, window.innerHeight - h - EDGE),
    })
  }, [])
  const onMoveEnd = useCallback(() => {
    move.current = null
  }, [])

  // --- resize floating (bottom-right grip) ---
  const rsz = useRef<{ sx: number; sy: number; sw: number; sh: number } | null>(null)
  const onResizeStart = useCallback(
    (e: React.PointerEvent) => {
      e.stopPropagation()
      rsz.current = { sx: e.clientX, sy: e.clientY, sw: size.w, sh: size.h }
      ;(e.target as HTMLElement).setPointerCapture?.(e.pointerId)
    },
    [size],
  )
  const onResizeDrag = useCallback((e: React.PointerEvent) => {
    if (!rsz.current) return
    const dx = e.clientX - rsz.current.sx
    const dy = e.clientY - rsz.current.sy
    // 上限还要减去窗口当前坐标，保证右/下边缘不越出视口。
    const { x, y } = geo.current.pos
    setSize({
      w: clamp(rsz.current.sw + dx, MIN_W, window.innerWidth - x - EDGE),
      h: clamp(rsz.current.sh + dy, MIN_H, window.innerHeight - y - EDGE),
    })
  }, [])
  const onResizeEnd = useCallback(() => {
    rsz.current = null
  }, [])

  // --- resize dock width (inner edge) ---
  const drsz = useRef<{ sx: number; sw: number } | null>(null)
  const onDockResizeStart = useCallback(
    (e: React.PointerEvent) => {
      e.stopPropagation()
      drsz.current = { sx: e.clientX, sw: dockW }
      ;(e.target as HTMLElement).setPointerCapture?.(e.pointerId)
    },
    [dockW],
  )
  const onDockResizeDrag = useCallback(
    (e: React.PointerEvent) => {
      if (!drsz.current) return
      const dx = e.clientX - drsz.current.sx
      // left dock: drag right grows width; right dock: drag left grows width.
      const next = mode === 'left' ? drsz.current.sw + dx : drsz.current.sw - dx
      setDockW(clamp(next, MIN_DOCK, MAX_DOCK))
    },
    [mode],
  )
  const onDockResizeEnd = useCallback(() => {
    drsz.current = null
  }, [])

  return createPortal(
    <div className="fixed inset-0 z-workspace">
      <div className="app-backdrop" onClick={onClose} />
      <WindowChrome
        mode={mode}
        pos={pos}
        size={size}
        dockW={dockW}
        treeW={treeW}
        setTreeW={setTreeW}
        setMode={setMode}
        onClose={onClose}
        moveProps={{ onPointerDown: onMoveStart, onPointerMove: onMoveDrag, onPointerUp: onMoveEnd }}
        resizeProps={{ onPointerDown: onResizeStart, onPointerMove: onResizeDrag, onPointerUp: onResizeEnd }}
        dockResizeProps={{ onPointerDown: onDockResizeStart, onPointerMove: onDockResizeDrag, onPointerUp: onDockResizeEnd }}
      />
    </div>,
    document.body,
  )
}

/* --------------------------------- chrome --------------------------------- */
function WindowChrome({
  mode,
  pos,
  size,
  dockW,
  treeW,
  setTreeW,
  setMode,
  onClose,
  moveProps,
  resizeProps,
  dockResizeProps,
}: {
  mode: WsMode
  pos: { x: number; y: number }
  size: { w: number; h: number }
  dockW: number
  treeW: number
  setTreeW: React.Dispatch<React.SetStateAction<number>>
  setMode: (m: WsMode) => void
  onClose?: () => void
  moveProps: PointerHandlers
  resizeProps: PointerHandlers
  dockResizeProps: PointerHandlers
}) {
  const { t } = useI18n()
  const positionClass =
    mode === 'left'
      ? 'left-0 top-0 h-full animate-slide-left'
      : mode === 'right'
        ? 'right-0 top-0 h-full animate-slide-right'
        : 'animate-scale-in'

  const floatingStyle =
    mode === 'floating'
      ? { left: pos.x, top: pos.y, width: size.w, height: size.h }
      : { width: clamp(dockW, MIN_DOCK, MAX_DOCK) }

  return (
    <div
      className={cn(
        'absolute flex flex-col overflow-hidden border border-border bg-surface shadow-pop',
        mode === 'floating' ? 'rounded-2xl' : mode === 'left' ? 'rounded-r-2xl border-l-0' : 'rounded-l-2xl border-r-0',
        positionClass,
      )}
      style={floatingStyle}
    >
      <Header mode={mode} setMode={setMode} onClose={onClose} dragProps={moveProps} />
      <div className="min-h-0 flex-1">
        <BrowserBody treeW={treeW} setTreeW={setTreeW} />
      </div>

      {/* dock inner-edge resize handle */}
      {mode !== 'floating' && (
        <div
          onPointerDown={dockResizeProps.onPointerDown}
          onPointerMove={dockResizeProps.onPointerMove}
          onPointerUp={dockResizeProps.onPointerUp}
          className={cn(
            'absolute top-0 bottom-0 w-1.5 cursor-col-resize bg-transparent transition-colors hover:bg-primary/30',
            mode === 'left' ? 'right-0' : 'left-0',
          )}
        />
      )}

      {/* floating bottom-right resize grip */}
      {mode === 'floating' && (
        <div
          onPointerDown={resizeProps.onPointerDown}
          onPointerMove={resizeProps.onPointerMove}
          onPointerUp={resizeProps.onPointerUp}
          className="absolute bottom-0 right-0 flex h-5 w-5 cursor-nwse-resize items-end justify-center text-muted/60 hover:text-primary"
          aria-label={t('workspace.resize')}
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none">
            <path d="M11 5 5 11M11 9 9 11" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
          </svg>
        </div>
      )}
    </div>
  )
}

interface PointerHandlers {
  onPointerDown: (e: React.PointerEvent) => void
  onPointerMove: (e: React.PointerEvent) => void
  onPointerUp: (e: React.PointerEvent) => void
}

function Header({
  mode,
  setMode,
  onClose,
  dragProps,
}: {
  mode: WsMode
  setMode: (m: WsMode) => void
  onClose?: () => void
  dragProps: PointerHandlers
}) {
  const { t } = useI18n()
  return (
    <div
      onPointerDown={dragProps.onPointerDown}
      onPointerMove={dragProps.onPointerMove}
      onPointerUp={dragProps.onPointerUp}
      className={cn(
        'flex shrink-0 items-center gap-2 border-b border-border bg-surface-2 px-3 py-2',
        mode === 'floating' ? 'cursor-grab active:cursor-grabbing' : 'cursor-default',
      )}
    >
      <Icon name="layers" size={16} className="shrink-0 text-primary" />
      <span className="font-display text-sm font-semibold text-text">{t('workspace.browse')}</span>

      <div data-no-drag className="ml-2 flex items-center gap-0.5 rounded-lg border border-border bg-surface p-0.5">
        <DockBtn active={mode === 'left'} icon="panel-left" label={t('workspace.dock_left')} onClick={() => setMode('left')} />
        <DockBtn active={mode === 'floating'} icon="move" label={t('workspace.dock_floating')} onClick={() => setMode('floating')} />
        <DockBtn active={mode === 'right'} icon="panel-right" label={t('workspace.dock_right')} onClick={() => setMode('right')} />
      </div>

      <div className="flex-1" />
      <span data-no-drag>
        <IconButton icon="close" label={t('workspace.close')} size="sm" onClick={onClose} className="text-muted" />
      </span>
    </div>
  )
}

function DockBtn({
  active,
  icon,
  label,
  onClick,
}: {
  active: boolean
  icon: string
  label: string
  onClick: () => void
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      aria-label={label}
      className={cn(
        'flex h-6 w-7 items-center justify-center rounded-md transition-colors',
        active ? 'bg-primary/15 text-primary' : 'text-muted hover:text-text',
      )}
    >
      <Icon name={icon} size={16} />
    </button>
  )
}

/* ------------------------------- browser body ------------------------------ */
function BrowserBody({ treeW, setTreeW }: { treeW: number; setTreeW: React.Dispatch<React.SetStateAction<number>> }) {
  const { theme } = useTheme()
  const { t } = useI18n()
  const { toast } = useNotifications()
  const hlReady = useHighlighter(theme)
  const [info, setInfo] = useState<WorkspaceInfo | null>(null)
  const [tree, setTree] = useState<TreeNode[]>([])
  const [rootLoading, setRootLoading] = useState(true)
  const [activePath, setActivePath] = useState<string | null>(null)
  const [file, setFile] = useState<FileContent | null>(null)
  const [fileLoading, setFileLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(new Set())

  // 文件树/预览分栏：树宽上限必须与分栏容器的**实测宽度**相关，否则在窄面板上
  // 分隔条会被推到面板之外（既不可见也无法再拖回），预览区同时被挤成 0 宽。
  const rowRef = useRef<HTMLDivElement>(null)
  const clampTreeW = useCallback((n: number) => {
    const rowW = rowRef.current?.clientWidth ?? 0
    const hi = rowW > 0 ? Math.max(TREE_MIN, Math.min(TREE_MAX, rowW - VIEWER_MIN)) : TREE_MAX
    return clamp(n, TREE_MIN, hi)
  }, [])

  // 面板变窄（切 dock/floating、拖窄、窗口缩小）时，把已保存的树宽一并收回，
  // 否则 localStorage 里的旧值会让预览区在窄面板上直接消失。
  useEffect(() => {
    const el = rowRef.current
    if (!el) return
    const apply = () => setTreeW((w) => clampTreeW(w))
    apply()
    const ro = new ResizeObserver(apply)
    ro.observe(el)
    return () => ro.disconnect()
  }, [setTreeW, clampTreeW])

  const loadRoot = useCallback(async () => {
    setRootLoading(true)
    setError(null)
    try {
      const [ws, list] = await Promise.all([fetchWorkspace(), fetchListing('.')])
      setInfo(ws)
      setTree(entriesToNodes(list.entries, ''))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setRootLoading(false)
    }
  }, [])

  useEffect(() => {
    void loadRoot()
  }, [loadRoot])

  const toggleDir = useCallback(
    async (node: TreeNode) => {
      const isOpen = expanded.has(node.path)
      const next = new Set(expanded)
      if (isOpen) {
        next.delete(node.path)
      } else {
        next.add(node.path)
        if (!node.loaded) {
          setTree((prev) => updateNode(prev, node.path, { loading: true }))
          try {
            const list = await fetchListing(node.path || '.')
            setTree((prev) =>
              updateNode(prev, node.path, {
                loading: false,
                loaded: true,
                children: entriesToNodes(list.entries, node.path),
              }),
            )
          } catch {
            setTree((prev) => updateNode(prev, node.path, { loading: false }))
            toast({ title: t('workspace.list_failed'), body: node.path, severity: 'warning' })
          }
        }
      }
      setExpanded(next)
    },
    [expanded],
  )

  const openFile = useCallback(async (node: TreeNode) => {
    setActivePath(node.path)
    setFile(null)
    setFileLoading(true)
    setError(null)
    try {
      setFile(await fetchFile(node.path))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setFileLoading(false)
    }
  }, [])

  // --- splitter (tree / viewer) ---
  const split = useRef<{ sx: number; sw: number } | null>(null)
  const onSplitDown = useCallback(
    (e: React.PointerEvent) => {
      split.current = { sx: e.clientX, sw: treeW }
      ;(e.target as HTMLElement).setPointerCapture?.(e.pointerId)
    },
    [treeW],
  )
  const onSplitMove = useCallback(
    (e: React.PointerEvent) => {
      if (!split.current) return
      setTreeW(clampTreeW(split.current.sw + (e.clientX - split.current.sx)))
    },
    [clampTreeW],
  )
  const onSplitUp = useCallback(() => {
    split.current = null
  }, [])

  const lang = activePath ? languageOf(activePath) : 'plaintext'
  // 全文高亮改为异步按需（useHighlightedCode）：语言 chunk 首次加载后注册，
  // 高亮不进主 bundle，流式期间经 deferred 合并。超大文本（>256KiB，尤其
  // highlightAuto 的多语言探测是主线程秒级阻塞）仍直接跳过高亮走纯文本路径。
  const skipHl = (file?.content?.length ?? 0) > 256 * 1024
  const html = useHighlightedCode(
    skipHl || !file?.content ? '' : file.content,
    skipHl ? 'plaintext' : lang,
  )

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-2 border-b border-border px-3 py-1.5">
        <span className="max-w-[60%] truncate text-2xs text-muted" title={info?.root}>
          {info ? info.root : t('workspace.loading')}
        </span>
        <div className="flex-1" />
        <Button size="icon-sm" variant="ghost" onClick={() => loadRoot()} aria-label={t('workspace.refresh')}>
          <Icon name="refresh" size={14} />
        </Button>
      </div>

      {error && (
        <div className="border-b border-danger/20 bg-danger/[0.06] px-3 py-1.5 text-2xs text-danger">{error}</div>
      )}

      <div ref={rowRef} className="relative flex min-h-0 flex-1">
        {/* Tree */}
        <div className="no-scrollbar shrink-0 overflow-y-auto border-r border-border p-2" style={{ width: treeW }}>
          {rootLoading ? (
            <div className="flex justify-center py-8">
              <Spinner className="text-muted" />
            </div>
          ) : (
            <ul className="space-y-0.5">
              {tree.map((n) => (
                <TreeRow
                  key={n.path}
                  node={n}
                  depth={0}
                  expanded={expanded}
                  activePath={activePath}
                  onToggle={toggleDir}
                  onOpen={openFile}
                />
              ))}
            </ul>
          )}
        </div>

        {/* splitter */}
        <div
          onPointerDown={onSplitDown}
          onPointerMove={onSplitMove}
          onPointerUp={onSplitUp}
          className="absolute top-0 bottom-0 z-raised w-1.5 -translate-x-1/2 cursor-col-resize bg-transparent transition-colors hover:bg-primary/30"
          style={{ left: treeW }}
          aria-label={t('workspace.resize_tree')}
        />

        {/* Viewer */}
        <div className="flex min-w-0 flex-1 flex-col">
          {activePath ? (
            <>
              <div className="flex items-center gap-2 border-b border-border bg-surface-2 px-3 py-1.5">
                <Icon name={fileIcon(activePath)} size={14} className="shrink-0 text-muted" />
                <span className="truncate font-mono text-xs text-text-2">{activePath}</span>
                {file && (
                  <Badge tone="neutral" className="ml-auto px-1.5 py-0 text-2xs">
                    {compact(file.size)} B
                  </Badge>
                )}
                <Badge tone="primary" className="px-1.5 py-0 text-2xs">
                  {lang}
                </Badge>
              </div>
              <div className="min-h-0 flex-1 overflow-auto bg-code-bg text-code-fg">
                {fileLoading ? (
                  <div className="flex justify-center py-12">
                    <Spinner className="text-muted" />
                  </div>
                ) : file?.binary ? (
                  <BinaryPlaceholder size={file.size} />
                ) : file?.truncated ? (
                  <div className="border-b border-warning/20 bg-warning/[0.06] px-3 py-1.5 text-2xs text-warning">
                    {t('workspace.too_large')}
                  </div>
                ) : null}
                {file?.content != null && !fileLoading && (
                  <CodeView html={html} language={lang} ready={hlReady && !skipHl} raw={file.content} />
                )}
              </div>
            </>
          ) : (
            <div className="flex flex-1 flex-col items-center justify-center px-6 text-center text-muted">
              <span className="mb-3 flex h-12 w-12 items-center justify-center rounded-xl bg-surface-2">
                <Icon name="file" size={20} />
              </span>
              <p className="text-sm font-medium text-text-2">{t('workspace.select_file')}</p>
              <p className="mt-1 max-w-xs text-xs">{t('workspace.resize_hint')}</p>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

function TreeRow({
  node,
  depth,
  expanded,
  activePath,
  onToggle,
  onOpen,
}: {
  node: TreeNode
  depth: number
  expanded: Set<string>
  activePath: string | null
  onToggle: (n: TreeNode) => void
  onOpen: (n: TreeNode) => void
}) {
  const isOpen = expanded.has(node.path)
  const isActive = activePath === node.path
  const isDir = node.kind === 'dir'

  return (
    <li>
      <button
        onClick={() => (isDir ? onToggle(node) : onOpen(node))}
        className={cn(
          'flex w-full items-center gap-1.5 rounded-md py-1 pr-2 text-left text-sm transition-colors',
          isActive ? 'bg-primary/10 text-primary' : 'text-text-2 hover:bg-surface-2',
        )}
        style={{ paddingLeft: treeIndent(depth) }}
      >
        {isDir ? (
          <>
            {node.loading ? (
              <Spinner size={12} className="text-muted" />
            ) : (
              <Icon
                name="chevron-right"
                size={14}
                className={cn('shrink-0 text-muted transition-transform', isOpen && 'rotate-90')}
              />
            )}
            <Icon name={isOpen ? 'folder-open' : 'folder'} size={16} className="shrink-0 text-primary/80" />
          </>
        ) : (
          <>
            <span className="w-[13px] shrink-0" />
            <Icon name={fileIcon(node.name)} size={14} className="shrink-0 text-muted" />
          </>
        )}
        <span className="truncate">{node.name}</span>
      </button>
      {isDir && isOpen && node.children && (
        <ul className="space-y-0.5">
          {node.children.map((c) => (
            <TreeRow
              key={c.path}
              node={c}
              depth={depth + 1}
              expanded={expanded}
              activePath={activePath}
              onToggle={onToggle}
              onOpen={onOpen}
            />
          ))}
        </ul>
      )}
    </li>
  )
}

function CodeView({
  html,
  language,
  ready,
  raw,
}: {
  html: string
  language: string
  ready: boolean
  raw: string
}) {
  const body = ready ? html : escapeHtml(raw)
  const lines = raw.split('\n')
  // 超大文件（数万行）：行号池改为单个文本节点（换行分隔），避免每行一个 <div>
  // 产生数万 DOM 节点导致渲染冻结。行不换行（pre），两列按行号天然对齐。
  const gutter = lines.length > 5000 ? lines.map((_, i) => i + 1).join('\n') : null
  return (
    <div className="flex min-w-full">
      <pre
        aria-hidden
        className="select-none border-r border-code-fg/10 px-3 py-3 text-right font-mono text-xs leading-[1.55] text-code-fg/50"
      >
        {gutter ?? lines.map((_, i) => <div key={i}>{i + 1}</div>)}
      </pre>
      <pre className="flex-1 overflow-x-auto px-3 py-3">
        <code
          className={cn('font-mono text-xs leading-[1.55]', language !== 'plaintext' && `language-${language}`)}
          dangerouslySetInnerHTML={{ __html: body }}
        />
      </pre>
    </div>
  )
}

function BinaryPlaceholder({ size }: { size: number }) {
  const { t } = useI18n()
  return (
    <div className="flex flex-col items-center justify-center px-6 py-12 text-center text-muted">
      <Icon name="file" size={26} className="mb-2 opacity-50" />
      <p className="text-sm font-medium text-text-2">{t('workspace.binary')}</p>
      <p className="mt-1 text-xs">{t('workspace.binary_desc', { size: compact(size) })}</p>
    </div>
  )
}

/* -------------------------------- helpers -------------------------------- */
function entriesToNodes(entries: FsEntry[], parent: string): TreeNode[] {
  return entries.map((e) => ({
    name: e.name,
    path: parent ? `${parent}/${e.name}` : e.name,
    kind: e.kind,
    size: e.size,
  }))
}

function updateNode(nodes: TreeNode[], path: string, patch: Partial<TreeNode>): TreeNode[] {
  return nodes.map((n) => {
    if (n.path === path) return { ...n, ...patch }
    if (n.children && path.startsWith(n.path + '/')) {
      return { ...n, children: updateNode(n.children, path, patch) }
    }
    return n
  })
}

function fileIcon(name: string): string {
  const lower = name.toLowerCase()
  if (lower.endsWith('.rs')) return 'cpu'
  if (lower.endsWith('.md')) return 'file'
  if (lower.match(/\.(ts|tsx|js|jsx|mjs|cjs)$/)) return 'file'
  if (lower.match(/\.(json|toml|yaml|yml|ini)$/)) return 'settings'
  if (lower.match(/\.(png|jpg|jpeg|gif|svg|webp)$/)) return 'image'
  if (lower === 'dockerfile' || lower.endsWith('.sh')) return 'terminal'
  return 'file'
}

function escapeHtml(s: string): string {
  // 字符 → HTML 实体。此前实现误把实体写成原字符（空操作）：未 ready 首帧渲染的
  // raw 文件内容经 dangerouslySetInnerHTML 注入会执行（XSS）。
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}
