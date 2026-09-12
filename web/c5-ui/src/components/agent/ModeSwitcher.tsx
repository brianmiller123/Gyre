import { Dropdown } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useSettings } from '@/lib/settings'
import { useI18n } from '@/lib/i18n'
import type { Mode } from '@/lib/settings'

/** 会话模式（镜像 CLI /mode）：只影响工具可用面，不改变会话身份。 */
const MODE_OPTIONS: { value: Mode; icon: string }[] = [
  { value: 'code', icon: 'cpu' },
  { value: 'architect', icon: 'layers' },
  { value: 'ask', icon: 'info' },
  { value: 'debug', icon: 'activity' },
]

/**
 * 输入区上方的模式选择器（Code / Architect / Ask / Debug）。
 *
 * 与模型、审批模式并列在同一行工具栏，三者共用 `.chip` 样式——它们是同一类
 * 事物（会话级上下文开关），此前却一个用原生 `<select>` 塞在输入框底部、另两个
 * 用下拉胶囊放在输入框上方，导致「同类控件两处分布 + 两套外观」。
 */
export function ModeSwitcher() {
  const { settings, update } = useSettings()
  const { switchMode } = useAgentSession()
  const { t } = useI18n()
  const active = MODE_OPTIONS.find((m) => m.value === settings.mode) ?? MODE_OPTIONS[0]

  return (
    <Dropdown
      align="left"
      direction="up"
      panelClassName="min-w-[15rem]"
      trigger={
        <button type="button" className="chip" aria-label={t('composer.mode_aria')}>
          <Icon name={active.icon} size={14} className="shrink-0 text-primary" />
          <span className="truncate font-medium text-text">{t(`composer.mode.${active.value}`)}</span>
          <Icon name="chevron-down" size={12} className="shrink-0 text-muted" />
        </button>
      }
      items={MODE_OPTIONS.map((m) => ({
        label: t(`composer.mode.${m.value}`),
        icon: m.icon,
        active: m.value === settings.mode,
        onClick: () => {
          update({ mode: m.value })
          switchMode(m.value)
        },
      }))}
    />
  )
}
