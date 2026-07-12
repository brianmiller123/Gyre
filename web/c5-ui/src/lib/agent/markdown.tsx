import {
  Fragment,
  memo,
  type ReactNode,
  useDeferredValue,
  useMemo,
  useState,
} from 'react'
import { Icon } from '@/components/icons'
import { cn } from '@/lib/cn'
import { useI18n } from '@/lib/i18n'
import { highlight } from '@/lib/agent/highlight'

/**
 * Compact, dependency-light Markdown renderer tuned for assistant output.
 * Supports: fenced code blocks (syntax-highlighted via the bundled
 * highlight.js, with a copy button + language label), inline code, **bold**,
 * *italic*, headings, ordered/unordered lists, task lists, blockquotes,
 * tables, horizontal rules, links, and paragraphs. Raw HTML is escaped.
 *
 * Streaming-friendly: parsing is memoized on the source text, and code-block
 * highlighting is deferred (`useDeferredValue`) so high-frequency token deltas
 * don't re-tokenize on every frame — the visible text still updates in real
 * time while the (more expensive) highlight pass is coalesced.
 */

/** Render inline markdown (bold / italic / code / links) into React nodes. */
function inline(text: string, keyPrefix: string): ReactNode[] {
  const nodes: ReactNode[] = []
  // Token regex: code, bold, italic, link
  const regex = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|(\[[^\]]+\]\([^)]+\))/g
  let last = 0
  let m: RegExpExecArray | null
  let i = 0
  while ((m = regex.exec(text)) !== null) {
    if (m.index > last) nodes.push(<Fragment key={`${keyPrefix}-t${i}`}>{text.slice(last, m.index)}</Fragment>)
    const tok = m[0]
    if (tok.startsWith('`')) {
      nodes.push(
        <code key={`${keyPrefix}-c${i}`} className="rounded bg-surface-3 px-1.5 py-0.5 font-mono text-[0.85em] text-primary">
          {tok.slice(1, -1)}
        </code>,
      )
    } else if (tok.startsWith('**')) {
      nodes.push(<strong key={`${keyPrefix}-b${i}`} className="font-semibold text-text">{tok.slice(2, -2)}</strong>)
    } else if (tok.startsWith('*')) {
      nodes.push(<em key={`${keyPrefix}-i${i}`}>{tok.slice(1, -1)}</em>)
    } else if (tok.startsWith('[')) {
      const mm = /\[([^\]]+)\]\(([^)]+)\)/.exec(tok)!
      nodes.push(
        <a key={`${keyPrefix}-l${i}`} href={mm[2]} target="_blank" rel="noreferrer" className="text-primary underline underline-offset-2 hover:opacity-80">
          {mm[1]}
        </a>,
      )
    }
    last = m.index + tok.length
    i++
  }
  if (last < text.length) nodes.push(<Fragment key={`${keyPrefix}-t-end`}>{text.slice(last)}</Fragment>)
  return nodes
}

interface Block {
  type: 'code' | 'h' | 'ul' | 'ol' | 'quote' | 'table' | 'hr' | 'p' | 'blank'
  level?: number
  lang?: string
  text?: string
  items?: string[]
  headers?: string[]
  rows?: string[][]
}

/**
 * Matches a GFM table separator row, e.g. `| :--- | ---: | :--: |`.
 * Accepts `-` and `=` runs; non-ASCII dash/pipe variants are normalized to
 * ASCII first (see `normalizeTableLine`) so CJK-flavored model output — full-
 * width pipe `｜` (U+FF5C), em/en dashes, or `=` separators — parses as a
 * table instead of falling through to a raw paragraph.
 */
const TABLE_SEP = /^\s*\|?(\s*:?[-=]+:?\s*\|)+\s*:?[-=]+:?\s*\|?\s*$/

/** Full-width pipe `｜` (U+FF5C) and box-drawing `│` (U+2502) → ASCII `|`. */
const PIPE_CHARS = /[\uFF5C\u2502]/g
/** Hyphen / en dash / em dash / horizontal bar / full-width hyphen / minus → ASCII `-`. */
const DASH_CHARS = /[\u2010-\u2015\uFF0D\u2212]/g

function normalizePipes(line: string): string {
  return line.replace(PIPE_CHARS, '|')
}
function normalizeTableLine(line: string): string {
  return normalizePipes(line).replace(DASH_CHARS, '-')
}

function splitTableRow(line: string): string[] {
  let inner = normalizePipes(line).trim()
  if (inner.startsWith('|')) inner = inner.slice(1)
  if (inner.endsWith('|')) inner = inner.slice(0, -1)
  return inner.split('|').map((c) => c.trim())
}

function isTableStart(lines: string[], i: number): boolean {
  if (i + 1 >= lines.length) return false
  // Normalize the header (for the pipe check) and the separator (for the
  // regex) so tables authored with full-width pipes / non-ASCII dashes parse.
  const header = normalizeTableLine(lines[i])
  const sep = normalizeTableLine(lines[i + 1].trim())
  return header.includes('|') && TABLE_SEP.test(sep)
}

function parse(md: string): Block[] {
  const lines = md.replace(/\r\n/g, '\n').split('\n')
  const blocks: Block[] = []
  let i = 0
  while (i < lines.length) {
    const line = lines[i]
    if (!line.trim()) {
      blocks.push({ type: 'blank' })
      i++
      continue
    }
    // fenced code
    const fence = /^```(\w+)?/.exec(line.trim())
    if (fence) {
      const lang = fence[1] || ''
      const buf: string[] = []
      i++
      while (i < lines.length && !/^```\s*$/.test(lines[i].trim())) {
        buf.push(lines[i])
        i++
      }
      i++ // skip closing fence
      blocks.push({ type: 'code', lang, text: buf.join('\n') })
      continue
    }
    // hr
    if (/^(-{3,}|\*{3,}|_{3,})$/.test(line.trim())) {
      blocks.push({ type: 'hr' })
      i++
      continue
    }
    // table (GFM): header + separator + rows
    if (isTableStart(lines, i)) {
      const headers = splitTableRow(lines[i])
      i += 2 // skip header + separator
      const rows: string[][] = []
      while (i < lines.length && lines[i].trim() && normalizePipes(lines[i]).includes('|')) {
        rows.push(splitTableRow(lines[i]))
        i++
      }
      blocks.push({ type: 'table', headers, rows })
      continue
    }
    // heading
    const h = /^(#{1,6})\s+(.*)$/.exec(line)
    if (h) {
      blocks.push({ type: 'h', level: h[1].length, text: h[2] })
      i++
      continue
    }
    // blockquote
    if (/^>\s?/.test(line)) {
      const buf: string[] = []
      while (i < lines.length && /^>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^>\s?/, ''))
        i++
      }
      blocks.push({ type: 'quote', text: buf.join('\n') })
      continue
    }
    // unordered list
    if (/^\s*[-*+]\s+/.test(line)) {
      const items: string[] = []
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*[-*+]\s+/, ''))
        i++
      }
      blocks.push({ type: 'ul', items })
      continue
    }
    // ordered list
    if (/^\s*\d+\.\s+/.test(line)) {
      const items: string[] = []
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*\d+\.\s+/, ''))
        i++
      }
      blocks.push({ type: 'ol', items })
      continue
    }
    // paragraph (gather until blank / structural boundary)
    const buf: string[] = [line]
    i++
    while (
      i < lines.length &&
      lines[i].trim() &&
      !/^```/.test(lines[i].trim()) &&
      !/^(#{1,6})\s+/.test(lines[i]) &&
      !/^>\s?/.test(lines[i]) &&
      !/^\s*[-*+]\s+/.test(lines[i]) &&
      !/^\s*\d+\.\s+/.test(lines[i]) &&
      !isTableStart(lines, i)
    ) {
      buf.push(lines[i])
      i++
    }
    blocks.push({ type: 'p', text: buf.join('\n') })
  }
  return blocks
}

export const Markdown = memo(function Markdown({
  children,
  className,
}: {
  children: string
  className?: string
}) {
  const blocks = useMemo(() => parse(children), [children])
  return (
    <div className={cn('space-y-3 text-[14px] leading-relaxed text-text-2', className)}>
      {blocks.map((b, idx) => {
        if (b.type === 'blank') return null
        if (b.type === 'hr') return <hr key={idx} className="border-border" />
        if (b.type === 'code') return <CodeBlock key={idx} lang={b.lang ?? ''} code={b.text ?? ''} />
        if (b.type === 'table')
          return <TableBlock key={idx} headers={b.headers ?? []} rows={b.rows ?? []} idx={idx} />
        if (b.type === 'h') {
          const sizes = ['text-xl', 'text-lg', 'text-base', 'text-base', 'text-sm', 'text-sm']
          return (
            <p key={idx} className={cn('font-display font-bold text-text', sizes[(b.level ?? 1) - 1])}>
              {inline(b.text ?? '', `h${idx}`)}
            </p>
          )
        }
        if (b.type === 'quote') {
          return (
            <blockquote key={idx} className="border-l-2 border-primary/40 bg-primary/[0.04] py-1 pl-3 text-text-2">
              {inline(b.text ?? '', `q${idx}`)}
            </blockquote>
          )
        }
        if (b.type === 'ul') {
          return (
            <ul key={idx} className="space-y-1 pl-1">
              {b.items!.map((it, j) => {
                const done = /^\[[xX]\]\s+/.test(it)
                const text = it.replace(/^\[[ xX]\]\s+/, '')
                return (
                  <li key={j} className="flex gap-2">
                    <span className="mt-[7px] h-1.5 w-1.5 shrink-0 rounded-full bg-muted" />
                    <span className={cn(done && 'text-muted line-through')}>{inline(text, `ul${idx}-${j}`)}</span>
                  </li>
                )
              })}
            </ul>
          )
        }
        if (b.type === 'ol') {
          return (
            <ol key={idx} className="space-y-1 pl-1">
              {b.items!.map((it, j) => (
                <li key={j} className="flex gap-2">
                  <span className="tabular mt-0.5 w-4 shrink-0 text-right text-xs font-semibold text-muted">{j + 1}.</span>
                  <span>{inline(it, `ol${idx}-${j}`)}</span>
                </li>
              ))}
            </ol>
          )
        }
        // paragraph
        return (
          <p key={idx} className="whitespace-pre-wrap break-words">
            {inline(b.text ?? '', `p${idx}`)}
          </p>
        )
      })}
    </div>
  )
})

function TableBlock({
  headers,
  rows,
  idx,
}: {
  headers: string[]
  rows: string[][]
  idx: number
}) {
  const colCount = headers.length
  return (
    <div className="overflow-x-auto rounded-xl border border-border">
      <table className="w-full border-collapse text-[13px]">
        <thead>
          <tr className="bg-surface-2/70">
            {headers.map((h, j) => (
              <th
                key={j}
                className="border-b border-border px-3 py-1.5 text-left font-semibold text-text"
              >
                {inline(h, `th${idx}-${j}`)}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i} className="border-b border-border/60 last:border-0">
              {Array.from({ length: colCount }).map((_, j) => (
                <td key={j} className="px-3 py-1.5 align-top text-text-2">
                  {inline(r[j] ?? '', `td${idx}-${i}-${j}`)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function CodeBlock({ lang, code }: { lang: string; code: string }) {
  // Defer the highlight pass during streaming so rapid token deltas don't
  // re-tokenize on every frame. `deferred` lags behind `code` by at most a few
  // frames (imperceptible), while the main thread stays responsive.
  const deferred = useDeferredValue(code)
  const html = useMemo(() => highlight(deferred, lang), [deferred, lang])
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const copy = () => {
    navigator.clipboard?.writeText(code).catch(() => {})
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1500)
  }
  return (
    <div className="group relative overflow-hidden rounded-xl border border-border text-[13px]">
      <div className="flex items-center justify-between border-b border-white/5 bg-black/20 px-3 py-1.5">
        <span className="font-mono text-[11px] uppercase tracking-wide text-white/40">{lang || 'code'}</span>
        <button
          onClick={copy}
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] text-white/50 transition-colors hover:bg-white/10 hover:text-white"
        >
          <Icon name={copied ? 'check' : 'copy'} size={12} /> {copied ? t('common.copied') : t('common.copy')}
        </button>
      </div>
      <pre className="hljs overflow-x-auto p-3">
        <code className="font-mono leading-relaxed" dangerouslySetInnerHTML={{ __html: html }} />
      </pre>
    </div>
  )
}
