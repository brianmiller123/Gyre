import { useEffect, useState } from 'react'
// highlight.js core + languages are bundled locally (no CDN at runtime).
import hljs from 'highlight.js/lib/core'
import 'highlight.js/styles/github-dark.min.css'

// Register the language set the file viewer may encounter. Each import is a
// static ES module so Vite bundles it; tree-shaking keeps the bundle lean.
import rust from 'highlight.js/lib/languages/rust'
import typescript from 'highlight.js/lib/languages/typescript'
import javascript from 'highlight.js/lib/languages/javascript'
import json from 'highlight.js/lib/languages/json'
import yaml from 'highlight.js/lib/languages/yaml'
import ini from 'highlight.js/lib/languages/ini'
import markdown from 'highlight.js/lib/languages/markdown'
import python from 'highlight.js/lib/languages/python'
import go from 'highlight.js/lib/languages/go'
import java from 'highlight.js/lib/languages/java'
import c from 'highlight.js/lib/languages/c'
import cpp from 'highlight.js/lib/languages/cpp'
import csharp from 'highlight.js/lib/languages/csharp'
import bash from 'highlight.js/lib/languages/bash'
import xml from 'highlight.js/lib/languages/xml'
import css from 'highlight.js/lib/languages/css'
import scss from 'highlight.js/lib/languages/scss'
import sql from 'highlight.js/lib/languages/sql'
import php from 'highlight.js/lib/languages/php'
import ruby from 'highlight.js/lib/languages/ruby'
import kotlin from 'highlight.js/lib/languages/kotlin'
import swift from 'highlight.js/lib/languages/swift'
import scala from 'highlight.js/lib/languages/scala'
import lua from 'highlight.js/lib/languages/lua'
import graphql from 'highlight.js/lib/languages/graphql'
import dockerfile from 'highlight.js/lib/languages/dockerfile'
import makefile from 'highlight.js/lib/languages/makefile'
import protobuf from 'highlight.js/lib/languages/protobuf'

let registered = false
function registerAll() {
  if (registered) return
  registered = true
  const langs: Record<string, any> = {
    rust, typescript, javascript, json, yaml, ini, markdown, python, go, java,
    c, cpp, csharp, bash, xml, css, scss, sql, php, ruby, kotlin, swift, scala,
    lua, graphql, dockerfile, makefile, protobuf,
  }
  for (const [name, def] of Object.entries(langs)) hljs.registerLanguage(name, def)
}
registerAll()

/**
 * With everything bundled, highlighting is available synchronously on mount.
 * The hook keeps a `ready` flag (true after first effect) so callers can defer
 * the first paint imperceptibly; no network is involved.
 *
 * The github-dark theme is imported statically and styles `.hljs`. The code
 * viewer always renders on a dark background, so a single theme suits both
 * light and dark app modes — no runtime theme swap needed.
 */
export function useHighlighter(_theme: 'light' | 'dark'): boolean {
  const [ready, setReady] = useState(false)
  useEffect(() => {
    registerAll()
    setReady(true)
  }, [])
  return ready
}

/** Highlight a code string for the given language; returns safe HTML. */
export function highlight(code: string, lang: string): string {
  registerAll()
  try {
    if (lang && lang !== 'plaintext' && hljs.getLanguage(lang)) {
      return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value
    }
    return hljs.highlightAuto(code).value
  } catch {
    return escapeHtml(code)
  }
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, '&').replace(/</g, '<').replace(/>/g, '>')
}
