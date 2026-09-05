/**
 * # Inline `@file` mention expansion
 *
 * Mirrors `crates/prompt/src/mentions.rs` (`agent_prompt::mentions`) so the Web
 * Composer and the Rust CLI expand `@file`-mentions into attached context blocks
 * with identical grammar and identical rendered format.
 *
 * - `@file <path>` (path is the next whitespace-delimited token) → injects the
 *   file body (`GET /api/file?path=<rel>`).
 *
 * Rendered as a context block appended after the user text. Parity is mandatory.
 */

/** A parsed mention. */
export type Mention = { kind: 'file'; path: string }

/**
 * Parse every `@file <path>` mention (deduped, first-seen order). Mirrors Rust
 * `parse_mentions` token-for-token:
 *
 * - Split by lines, then by whitespace.
 * - A standalone `@file` token consumes the next whitespace-delimited token as
 *   the path; surrounding `"`/`'` chars are stripped (any leading/trailing run).
 * - Unrecognized `@xxx` tokens are ignored.
 */
export function parseMentions(text: string): Mention[] {
  const out: Mention[] = []
  const hasFile = (p: string) => out.some((m) => m.kind === 'file' && m.path === p)
  for (const line of text.split('\n')) {
    const tokens = line.split(/\s+/).filter((t) => t.length > 0)
    for (let i = 0; i < tokens.length; i++) {
      const tok = tokens[i]
      if (tok.toLowerCase() === '@file') {
        const next = tokens[i + 1]
        if (next !== undefined) {
          // Strip any leading/trailing run of `"` or `'` (Rust trim_matches).
          const cleaned = next.replace(/^["']+|["']+$/g, '')
          if (!hasFile(cleaned)) out.push({ kind: 'file', path: cleaned })
          i += 1 // consume the path token
        }
      }
    }
  }
  return out
}

/**
 * Markdown fence language for an extension (mirrors Rust `fence_lang`).
 * Returns `''` for unknown extensions so the fence renders as a plain block.
 */
export function fenceLang(path: string): string {
  // Everything after the last `.`; a dotless path yields the whole name (also
  // unmatched → ''), matching Rust `rsplit('.').next()`.
  const dot = path.lastIndexOf('.')
  const ext = (dot === -1 ? path : path.slice(dot + 1)).toLowerCase()
  switch (ext) {
    case 'rs':
      return 'rust'
    case 'toml':
      return 'toml'
    case 'ts':
    case 'tsx':
      return 'ts'
    case 'js':
    case 'jsx':
    case 'mjs':
    case 'cjs':
      return 'js'
    case 'py':
      return 'python'
    case 'go':
      return 'go'
    case 'java':
      return 'java'
    case 'c':
    case 'h':
      return 'c'
    case 'cpp':
    case 'cc':
    case 'hpp':
      return 'cpp'
    case 'rb':
      return 'ruby'
    case 'sh':
    case 'bash':
    case 'zsh':
      return 'bash'
    case 'json':
      return 'json'
    case 'yaml':
    case 'yml':
      return 'yaml'
    case 'md':
      return 'md'
    case 'html':
      return 'html'
    case 'css':
      return 'css'
    default:
      return ''
  }
}

/** Render a single file context block (mirrors Rust `format_file_block`). */
export function formatFileBlock(path: string, content: string): string {
  return `<file path="${path}">\n\`\`\`${fenceLang(path)}\n${content}\n\`\`\`\n</file>`
}

/**
 * Append rendered context blocks after the user text. With no blocks the user
 * text is returned unchanged (mirrors Rust `render_attached`).
 */
export function renderAttached(userText: string, blocks: string[]): string {
  if (blocks.length === 0) return userText
  return `${userText}\n\n--- attached context ---\n\n${blocks.join('\n\n')}`
}

/** `/api/file` response shape (verified in `crates/server/src/lib.rs::read_file`). */
interface FileBody {
  path: string
  binary: boolean
  size: number
  truncated: boolean
  content: string | null
}

/** Context passed to {@link expandMentions}. */
export interface ExpandCtx {
  /** Authenticated GET helper (adds `?token=` / `&token=`). */
  apiGet: <T>(path: string) => Promise<T | null>
  /** Optional toast for skipped/failed attachments. Tolerant when absent. */
  say?: (text: string, level?: string) => void
  /** i18n translator（警告文案本地化必需，调用方必须传入）。 */
  t: (key: string, args?: Record<string, unknown>) => string
}

/**
 * Expand every `@file`-mention into attached context blocks and append them to
 * the user text. Tolerant: a failed/binary/missing file is skipped with an
 * optional `say` warning rather than aborting the send.
 *
 * Returns the original text unchanged when there are no mentions.
 */
export async function expandMentions(text: string, ctx: ExpandCtx): Promise<string> {
  const mentions = parseMentions(text)
  if (mentions.length === 0) return text
  const blocks: string[] = []
  for (const m of mentions) {
    const data = await ctx.apiGet<FileBody>(`/api/file?path=${encodeURIComponent(m.path)}`)
    if (data && data.content !== null && !data.binary) {
      blocks.push(formatFileBlock(m.path, data.content))
    } else {
      ctx.say?.(ctx.t('mentions.attach_failed', { path: m.path }), 'warning')
    }
  }
  return renderAttached(text, blocks)
}
