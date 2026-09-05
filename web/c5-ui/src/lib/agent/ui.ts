import type { Tone } from '@/components/ui'

/**
 * Agent 状态机 → 徽标 tone / i18n key。
 *
 * label / desc 存的是 locales 键（`state.*`），渲染方必须过一层 `t()`——
 * 直接输出会把中文泄漏给 en/ru/ja 用户。
 */
export const stateMeta: Record<
  string,
  { label: string; tone: Tone; dot?: boolean; desc: string }
> = {
  no_task: { label: 'state.no_task.label', tone: 'neutral', desc: 'state.no_task.desc' },
  running: { label: 'state.running.label', tone: 'warning', dot: true, desc: 'state.running.desc' },
  streaming: { label: 'state.streaming.label', tone: 'primary', dot: true, desc: 'state.streaming.desc' },
  waiting_for_input: { label: 'state.waiting_for_input.label', tone: 'info', dot: true, desc: 'state.waiting_for_input.desc' },
  idle: { label: 'state.idle.label', tone: 'success', desc: 'state.idle.desc' },
  resumable: { label: 'state.resumable.label', tone: 'neutral', desc: 'state.resumable.desc' },
}

/** `say` level → icon + tone（纯展示样式，无文案）。 */
export const levelMeta: Record<string, { icon: string; tone: Tone }> = {
  info: { icon: 'info', tone: 'info' },
  thinking: { icon: 'activity', tone: 'neutral' },
  success: { icon: 'check-circle', tone: 'success' },
  warning: { icon: 'alert', tone: 'warning' },
  error: { icon: 'x-circle', tone: 'danger' },
  err: { icon: 'x-circle', tone: 'danger' },
}

/** AskKind 工具变体的内层工具名（{ tool: { tool: string } }，与后端序列化对齐）。 */
function toolKindName(tool: unknown): string {
  if (tool && typeof tool === 'object' && 'tool' in tool) {
    const name = tool.tool
    if (typeof name === 'string') return name
  }
  return ''
}

/** AskKind → 本地化标签。`t` 由调用方注入（组件内 useI18n）。 */
export function askKindLabel(
  kind: unknown,
  t: (key: string, args?: Record<string, unknown>) => string,
): string {
  if (typeof kind === 'string') {
    return kind === 'followup' ? t('ask.followup') : kind === 'completion_result' ? t('ask.completion_result') : kind
  }
  if (kind && typeof kind === 'object') {
    if ('tool' in kind) return t('ask.tool', { tool: toolKindName(kind.tool) })
    if ('command' in kind) return t('ask.command')
  }
  return t('ask.confirm')
}
