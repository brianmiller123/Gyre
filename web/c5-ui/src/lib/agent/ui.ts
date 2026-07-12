import type { Tone } from '@/components/ui'

/** Agent state machine → label / tone / description for badges. */
export const stateMeta: Record<
  string,
  { label: string; tone: Tone; dot?: boolean; desc: string }
> = {
  no_task: { label: '空闲', tone: 'neutral', desc: '等待新任务' },
  running: { label: '运行中', tone: 'warning', dot: true, desc: '智能体正在推理' },
  streaming: { label: '生成中', tone: 'primary', dot: true, desc: '正在流式输出' },
  waiting_for_input: { label: '等待审批', tone: 'info', dot: true, desc: '需要你的确认' },
  idle: { label: '已完成', tone: 'success', desc: '任务结束' },
  resumable: { label: '可恢复', tone: 'neutral', desc: '已暂停' },
}

/** `say` level → icon + tone. */
export const levelMeta: Record<string, { icon: string; tone: Tone; label: string }> = {
  info: { icon: 'info', tone: 'info', label: '信息' },
  thinking: { icon: 'activity', tone: 'neutral', label: '思考' },
  success: { icon: 'check-circle', tone: 'success', label: '成功' },
  warning: { icon: 'alert', tone: 'warning', label: '警告' },
  error: { icon: 'x-circle', tone: 'danger', label: '错误' },
  err: { icon: 'x-circle', tone: 'danger', label: '错误' },
}

/** Human label for an AskKind. */
export function askKindLabel(kind: unknown): string {
  if (typeof kind === 'string') {
    return kind === 'followup' ? '追问' : kind === 'completion_result' ? '完成确认' : kind
  }
  if (kind && typeof kind === 'object') {
    if ('tool' in (kind as any)) return `工具审批 · ${(kind as any).tool?.tool ?? ''}`
    if ('command' in (kind as any)) return '命令审批'
  }
  return '需要确认'
}
