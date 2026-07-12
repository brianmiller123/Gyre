/** Types for the read-only file-browser endpoints (`/api/workspace|fs|file`). */
export interface WorkspaceInfo {
  root: string
  name: string
}
export interface FsEntry {
  name: string
  kind: 'dir' | 'file'
  size: number
}
export interface FsListing {
  path: string
  entries: FsEntry[]
}
export interface FileContent {
  path: string
  binary: boolean
  size: number
  truncated: boolean
  content: string | null
}

/**
 * Connection context for the workspace API. The agent-session provider binds
 * these on mount / whenever settings change, so the plain (non-hook) fetch
 * helpers below can target the live server + carry the auth token.
 */
let boundOrigin = ''
let boundToken = ''

export function bindWorkspaceContext(serverUrl: string, token: string) {
  boundOrigin = serverUrl.replace(/\/$/, '')
  boundToken = token
}

function withToken(extra: Record<string, string>): string {
  const params = new URLSearchParams(extra)
  if (boundToken) params.set('token', boundToken)
  const s = params.toString()
  return s ? `?${s}` : ''
}

export async function fetchWorkspace(): Promise<WorkspaceInfo> {
  const q = boundToken ? `?token=${encodeURIComponent(boundToken)}` : ''
  const r = await fetch(`${boundOrigin}/api/workspace${q}`)
  if (!r.ok) throw new Error(`workspace (HTTP ${r.status})`)
  return r.json()
}

export async function fetchListing(path: string): Promise<FsListing> {
  const r = await fetch(`${boundOrigin}/api/fs${withToken({ path })}`)
  if (!r.ok) throw new Error(`fs ${path} (HTTP ${r.status})`)
  return r.json()
}

export async function fetchFile(path: string): Promise<FileContent> {
  const r = await fetch(`${boundOrigin}/api/file${withToken({ path })}`)
  if (!r.ok) throw new Error(`file ${path} (HTTP ${r.status})`)
  return r.json()
}

/** Map a filename to a highlight.js language id (best-effort). */
export function languageOf(name: string): string {
  const lower = name.toLowerCase()
  const ext = lower.includes('.') ? lower.split('.').pop()! : ''
  const map: Record<string, string> = {
    rs: 'rust', ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript',
    mjs: 'javascript', cjs: 'javascript', json: 'json', toml: 'ini',
    yaml: 'yaml', yml: 'yaml', md: 'markdown', markdown: 'markdown',
    py: 'python', go: 'go', java: 'java', kt: 'kotlin', c: 'c', h: 'c',
    cpp: 'cpp', cc: 'cpp', hpp: 'cpp', cs: 'csharp', rb: 'ruby',
    php: 'php', swift: 'swift', sh: 'bash', bash: 'bash', zsh: 'bash',
    fish: 'bash', html: 'xml', htm: 'xml', xml: 'xml', css: 'css',
    scss: 'scss', sql: 'sql', graphql: 'graphql', lua: 'lua',
    scala: 'scala', clj: 'clojure', ex: 'elixir', exs: 'elixir',
    erl: 'erlang', hs: 'haskell', vue: 'xml', svelte: 'xml', proto: 'protobuf',
  }
  if (lower === 'dockerfile') return 'dockerfile'
  if (lower === 'makefile') return 'makefile'
  if (lower.endsWith('.lock') || lower === 'cargo.lock') return 'json'
  if (lower.endsWith('.mod') || lower.endsWith('.sum')) return 'ini'
  return map[ext] ?? 'plaintext'
}
