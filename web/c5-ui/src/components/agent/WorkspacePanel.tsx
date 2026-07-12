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
import { highlight, useHighlighter } from '@/lib/agent/highlight'
import { useTheme } from '@/lib/theme'
import { Icon } from '@/components/icons'
import { Badge, Button, Spinner } from '@/components/ui'
import { compact } from '@/lib/format'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'

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
  const [pos, setPos] = useState<{ x: number; y: number }>(
    saved.current.pos ?? { x: clamp((vw - 760) / 2, 16, vw), y: 64 },
  )
  const [size, setSize] = useState<{ w: number; h: number }>(
    saved.current.size ?? { w: clamp(760, MIN_W, vw - 32), h: clamp(640, MIN_H, vh - 96) },
  )
  const [dockW, setDockW] = useState<number>(saved.current.dockW ?? 360)
  const [treeW, setTreeW] = useState<number>(saved.current.treeW ?? 208)

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

  // Keep floating window in-bounds on viewport resize.
  useEffect(() => {
    if (mode !== 'floating') return
    const onResize = () => {
      setSize((s) => ({ w: Math.min(s.w, window.innerWidth - 32), h: Math.min(s.h, window.innerHeight - 80) }))
      setPos((p) => ({ x: Math.min(p.x, window.innerWidth - 160), y: Math.min(p.y, window.innerHeight - 120) }))
    }
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
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
    setPos({
      x: clamp(move.current.px + dx, 8, window.innerWidth - 120),
      y: clamp(move.current.py + dy, 8, window.innerHeight - 64),
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
    setSize({
      w: clamp(rsz.current.sw + dx, MIN_W, window.innerWidth - 16),
      h: clamp(rsz.current.sh + dy, MIN_H, window.innerHeight - 16),
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
    <div className="fixed inset-0 z-[95]">
      <div className="absolute inset-0 bg-black/45 backdrop-blur-sm animate-fade-in" onClick={onClose} />
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
  setTreeW: (n: number) => void
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
        'flex shrink-0 items-center gap-2 border-b border-border bg-surface-2/70 px-3 py-2',
        mode === 'floating' ? 'cursor-grab active:cursor-grabbing' : 'cursor-default',
      )}
    >
      <Icon name="layers" size={15} className="shrink-0 text-primary" />
      <span className="font-display text-[13px] font-semibold text-text">{t('workspace.browse')}</span>

      <div data-no-drag className="ml-2 flex items-center gap-0.5 rounded-lg border border-border bg-surface p-0.5">
        <DockBtn active={mode === 'left'} icon="panel-left" label={t('workspace.dock_left')} onClick={() => setMode('left')} />
        <DockBtn active={mode === 'floating'} icon="move" label={t('workspace.dock_floating')} onClick={() => setMode('floating')} />
        <DockBtn active={mode === 'right'} icon="panel-right" label={t('workspace.dock_right')} onClick={() => setMode('right')} />
      </div>

      <div className="flex-1" />
      <button
        data-no-drag
        onClick={onClose}
        className="flex h-7 w-7 items-center justify-center rounded-md text-muted transition-colors hover:bg-surface-3 hover:text-text"
        aria-label={t('workspace.close')}
      >
        <Icon name="close" size={16} />
      </button>
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
      <Icon name={icon} size={15} />
    </button>
  )
}

/* ------------------------------- browser body ------------------------------ */
function BrowserBody({ treeW, setTreeW }: { treeW: number; setTreeW: (n: number) => void }) {
  const { theme } = useTheme()
  const { t } = useI18n()
  const hlReady = useHighlighter(theme)
  const [info, setInfo] = useState<WorkspaceInfo | null>(null)
  const [tree, setTree] = useState<TreeNode[]>([])
  const [rootLoading, setRootLoading] = useState(true)
  const [activePath, setActivePath] = useState<string | null>(null)
  const [file, setFile] = useState<FileContent | null>(null)
  const [fileLoading, setFileLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(new Set())

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
  const onSplitMove = useCallback((e: React.PointerEvent) => {
    if (!split.current) return
    setTreeW(clamp(split.current.sw + (e.clientX - split.current.sx), 140, 480))
  }, [setTreeW])
  const onSplitUp = useCallback(() => {
    split.current = null
  }, [])

  const lang = activePath ? languageOf(activePath) : 'plaintext'
  const html = file?.content ? highlight(file.content, lang) : ''

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-2 border-b border-border px-3 py-1.5">
        <span className="max-w-[60%] truncate text-[11px] text-muted" title={info?.root}>
          {info ? info.root : t('workspace.loading')}
        </span>
        <div className="flex-1" />
        <Button size="icon-sm" variant="ghost" onClick={() => loadRoot()} aria-label={t('workspace.refresh')}>
          <Icon name="refresh" size={14} />
        </Button>
      </div>

      {error && (
        <div className="border-b border-danger/20 bg-danger/[0.06] px-3 py-1.5 text-[11px] text-danger">{error}</div>
      )}

      <div className="relative flex min-h-0 flex-1">
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
          className="absolute top-0 bottom-0 z-10 w-1.5 -translate-x-1/2 cursor-col-resize bg-transparent transition-colors hover:bg-primary/30"
          style={{ left: treeW }}
          aria-label={t('workspace.resize_tree')}
        />

        {/* Viewer */}
        <div className="flex min-w-0 flex-1 flex-col">
          {activePath ? (
            <>
              <div className="flex items-center gap-2 border-b border-border bg-surface-2/50 px-3 py-1.5">
                <Icon name={fileIcon(activePath)} size={14} className="shrink-0 text-muted" />
                <span className="truncate font-mono text-[12px] text-text-2">{activePath}</span>
                {file && (
                  <Badge tone="neutral" className="ml-auto px-1.5 py-0 text-[10px]">
                    {compact(file.size)} B
                  </Badge>
                )}
                <Badge tone="primary" className="px-1.5 py-0 text-[10px]">
                  {lang}
                </Badge>
              </div>
              <div className="no-scrollbar min-h-0 flex-1 overflow-auto bg-[#0b0d12] dark:bg-[#070809]">
                {fileLoading ? (
                  <div className="flex justify-center py-12">
                    <Spinner className="text-muted" />
                  </div>
                ) : file?.binary ? (
                  <BinaryPlaceholder size={file.size} />
                ) : file?.truncated ? (
                  <div className="border-b border-warning/20 bg-warning/[0.06] px-3 py-1.5 text-[11px] text-warning">
                    {t('workspace.too_large')}
                  </div>
                ) : null}
                {file?.content != null && !fileLoading && (
                  <CodeView html={html} language={lang} ready={hlReady} raw={file.content} />
                )}
              </div>
            </>
          ) : (
            <div className="flex flex-1 flex-col items-center justify-center px-6 text-center text-muted">
              <span className="mb-3 flex h-12 w-12 items-center justify-center rounded-xl bg-surface-2">
                <Icon name="file" size={22} />
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
          'flex w-full items-center gap-1.5 rounded-md py-1 pr-2 text-left text-[13px] transition-colors',
          isActive ? 'bg-primary/10 text-primary' : 'text-text-2 hover:bg-surface-2',
        )}
        style={{ paddingLeft: depth * 12 + 4 }}
      >
        {isDir ? (
          <>
            {node.loading ? (
              <Spinner size={11} className="text-muted" />
            ) : (
              <Icon
                name="chevron-right"
                size={13}
                className={cn('shrink-0 text-muted transition-transform', isOpen && 'rotate-90')}
              />
            )}
            <Icon name={isOpen ? 'folder-open' : 'folder'} size={15} className="shrink-0 text-primary/80" />
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
  return (
    <div className="flex min-w-full">
      <pre
        aria-hidden
        className="select-none border-r border-white/5 px-3 py-3 text-right font-mono text-[12px] leading-[1.55] text-white/25"
      >
        {lines.map((_, i) => (
          <div key={i}>{i + 1}</div>
        ))}
      </pre>
      <pre className="flex-1 overflow-x-auto px-3 py-3">
        <code
          className={cn('font-mono text-[12px] leading-[1.55]', language !== 'plaintext' && `language-${language}`)}
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
  return s.replace(/&/g, '&').replace(/</g, '<').replace(/>/g, '>')
}
