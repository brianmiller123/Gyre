import { useEffect, useState } from 'react'
import {
  Modal,
  Field,
  Input,
  Select,
  Button,
  Divider,
  Switch,
  SectionLabel,
  Tabs,
} from '@/components/ui'
import { Icon } from '@/components/icons'
import { useSettings } from '@/lib/settings'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useTheme } from '@/lib/theme'
import { useNotifications } from '@/lib/notifications'
import { useI18n } from '@/lib/i18n'
import { SUPPORTED_LOCALES } from '@/lib/locales'
import type { LocaleCode } from '@/lib/locales'
import { cn } from '@/lib/cn'
import type { SettingsTab } from '@/components/agent/AgentShell'

/**
 * 强调色表。
 *
 * 每个 accent 只描述**浅色主题**下的取值；暗色主题的色阶由 `applyAccent` 现场向
 * 白色混合派生。旧实现把同一组值同时写进两个主题（且是行内样式，优先于 `.dark`
 * 规则），于是暗色主题的主色被浅色主题的深色值覆盖；叠加硬编码的前景色
 * `#06241f`，选 Rose / Indigo 这类深色 accent 就会出现「深底深字」。
 *
 * `primary` 逐个筛过对比度：要么能承载白字、要么能承载深墨字。两个被下调的取值：
 * Emerald `5 150 105`（白字 3.8:1）→ `4 120 87`（5.5:1）；
 * Teal `13 148 136`（白 3.8:1 / 墨 4.4:1，两侧都不达标）→ `15 118 110`（白 5.5:1）。
 */
const accents = [
  { name: 'Teal', primary: '15 118 110', glow: '45 212 191' },
  { name: 'Indigo', primary: '79 70 229', glow: '129 140 248' },
  { name: 'Blue', primary: '37 99 235', glow: '96 165 250' },
  { name: 'Emerald', primary: '4 120 87', glow: '52 211 153' },
  { name: 'Violet', primary: '124 58 237', glow: '167 139 250' },
  { name: 'Rose', primary: '225 29 72', glow: '251 113 133' },
  { name: 'Amber', primary: '217 119 6', glow: '251 191 36' },
]

const ACCENT_STORAGE = 'agent-accent'
/** 深墨前景（原硬编码 `#06241f`，即 dark 主题下的 on-primary）。 */
const INK = '6 36 31'

/** sRGB 相对亮度（WCAG 2.x 定义）。 */
function luminance(triple: string) {
  const channel = (v: number) => {
    const s = v / 255
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4)
  }
  const [r, g, b] = triple.split(' ').map(Number)
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}
const contrastWithWhite = (t: string) => 1.05 / (luminance(t) + 0.05)
const contrastWithInk = (t: string) => (luminance(t) + 0.05) / (luminance(INK) + 0.05)
/** 择优前景：白 / 深墨取对比度更高者，保证任意 accent 都能读出按钮文字。 */
const bestForeground = (t: string) =>
  contrastWithWhite(t) >= contrastWithInk(t) ? '255 255 255' : INK
/** 向白色混合：t=0 原色，t=1 纯白。 */
function lighten(triple: string, t: number) {
  return triple
    .split(' ')
    .map(Number)
    .map((v) => Math.round(v + (255 - v) * t))
    .join(' ')
}
function readStoredAccent() {
  try {
    return localStorage.getItem(ACCENT_STORAGE) || accents[0].name
  } catch {
    return accents[0].name
  }
}

type Accent = (typeof accents)[number]

/**
 * 把 accent 表项解析为**当前主题下**实际生效的色阶。
 *
 * 抽成纯函数是为了让「色板预览的小圆点」与「真正写进 CSS 变量的值」来自同一处
 * 计算——否则暗色主题下预览会显示未提亮的原色，与套用结果不符。
 */
function resolveAccent(a: Accent, dark: boolean) {
  return {
    primary: dark ? lighten(a.primary, 0.32) : a.primary,
    glow: dark ? lighten(a.glow, 0.16) : a.glow,
  }
}

/**
 * 唯一的偏好设置面板（连接 / 外观两个标签页）。
 *
 * 合并依据：主题与语言此前同时存在于侧栏脚注与设置面板，两套控件维护同一份
 * state，不仅视觉重复，键盘可达路径也不一致（侧栏自绘菜单 vs. 原生 select）。
 * 现在偏好只有一个归属地；侧栏只保留「打开设置」这一个入口。
 *
 * 「数据 / 清空对话」区块已移除：清空是**会话生命周期**动作而非偏好，唯一常驻
 * 入口在顶栏 ⋮ 溢出菜单（另有 /clear 斜杠命令），因此不再需要第二个确认弹窗。
 */
export function SettingsPanel({
  open,
  tab,
  onTabChange,
  onClose,
}: {
  open: boolean
  tab: SettingsTab
  onTabChange: (tab: SettingsTab) => void
  onClose: () => void
}) {
  const { settings, update } = useSettings()
  const { disconnect, connect, sessionId, socks5Status, refreshSocks5Status, setSocks5Enabled } =
    useAgentSession()
  const { theme, setTheme } = useTheme()
  const { toast } = useNotifications()
  const { t, preference, setPreference } = useI18n()

  const [draft, setDraft] = useState(settings)
  const [accent, setAccent] = useState(readStoredAccent)
  const [testing, setTesting] = useState(false)
  // SOCKS5 开关在途状态（防止连点；请求失败 toast 提示）。
  const [socks5Busy, setSocks5Busy] = useState(false)
  // SOCKS5 状态拉取过程/失败标记：与「服务端未配置」严格区分，避免误导读屏与用户。
  const [socks5Loading, setSocks5Loading] = useState(false)
  const [socks5LoadError, setSocks5LoadError] = useState(false)

  // URL 即时内联校验：随输入给出错误态，而非等到保存时 toast（Field/Input 的
  // error/invalid 能力此前一直闲置）。空值合法（保存时回退当前源）。
  const urlError = (() => {
    const raw = draft.serverUrl.trim()
    if (!raw) return null
    try {
      const u = new URL(raw)
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return t('settings.invalid_url')
      return null
    } catch {
      return t('settings.invalid_url')
    }
  })()

  useEffect(() => {
    if (open) {
      setDraft(settings)
      // 打开面板时主动刷新代理状态（不依赖 WS 连接），保证控件即时显示。
      setSocks5Loading(true)
      void refreshSocks5Status().then((ok) => {
        setSocks5Loading(false)
        setSocks5LoadError(!ok)
      })
    }
    // 仅依赖 open 上升沿：面板打开期间 settings 的身份变化（如刷新回写）不得重置
    // 草稿或触发重取——曾因把 settings 放进依赖造成无限刷新循环 + 用户输入被周期性回滚。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // accent × 主题 → CSS 变量。必须依赖 theme：暗色主题的色阶是现场派生的，
  // 否则「先选 accent 再切主题」会留下浅色主题的深色值。
  useEffect(() => {
    applyAccent(accent)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [accent, theme])

  function applyAccent(name: string) {
    const a = accents.find((x) => x.name === name) ?? accents[0]
    const root = document.documentElement
    const { primary, glow } = resolveAccent(a, root.classList.contains('dark'))
    root.style.setProperty('--c-primary', primary)
    root.style.setProperty('--c-primary-glow', glow)
    root.style.setProperty('--c-on-primary', bestForeground(primary))
    try {
      localStorage.setItem(ACCENT_STORAGE, name)
    } catch {
      /* storage 不可用（隐私模式）时忽略：本次会话内仍然生效 */
    }
    setAccent(name)
  }

  async function testConnection() {
    setTesting(true)
    try {
      const origin = draft.serverUrl.replace(/\/$/, '')
      const q = draft.token ? `?token=${encodeURIComponent(draft.token)}` : ''
      const r = await fetch(`${origin}/api/stats${q}`)
      if (!r.ok) throw new Error(`HTTP ${r.status}`)
      const d = await r.json()
      toast({
        title: t('settings.test_ok_title'),
        body: t('settings.test_ok_body', {
          sessions: d.active_sessions ?? '?',
          models: d.models_available ?? '?',
        }),
        severity: 'success',
      })
    } catch (e) {
      toast({
        title: t('settings.test_fail'),
        body: e instanceof Error ? e.message : String(e),
        severity: 'danger',
      })
    } finally {
      setTesting(false)
    }
  }

  function save() {
    // 校验并归一服务地址：非法值绝不入库（曾因 Sidebar 渲染期 new URL() 抛异常导致全屏白屏）。
    // 复用输入期的 urlError 校验结果，保存与输入两个入口规则永远一致。
    if (urlError) {
      toast({ title: t('settings.invalid_url'), body: draft.serverUrl.trim(), severity: 'danger' })
      return
    }
    const raw = draft.serverUrl.trim()
    const next = raw ? raw : window.location.origin
    // 同源保存时保留当前会话（resume 重建连接，model/mode 覆盖随查询串生效）；
    // 跨服务器保存则开新会话（旧会话 id 在新服务器上不存在）。
    const sameOrigin = settings.serverUrl.replace(/\/$/, '') === next.replace(/\/$/, '')
    const resume = sameOrigin ? (sessionId ?? undefined) : undefined
    update({ ...draft, serverUrl: next })
    toast({ title: t('settings.saved'), severity: 'success' })
    disconnect()
    setTimeout(() => void connect(resume), 60)
    onClose()
  }

  const isConnection = tab === 'connection'

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={t('settings.title')}
      description={t('settings.desc')}
      icon="settings"
      size="lg"
      footer={
        isConnection ? (
          <>
            <Button variant="secondary" onClick={onClose}>
              {t('settings.cancel')}
            </Button>
            <Button variant="primary" leftIcon="check" onClick={save}>
              {t('settings.save_reconnect')}
            </Button>
          </>
        ) : (
          <Button variant="secondary" onClick={onClose}>
            {t('common.close')}
          </Button>
        )
      }
    >
      {/* 标签页：把「连接」这类低频高风险表单与「外观」这类高频即时预览分开，
          避免用户在一条长滚动里定位区块（旧版三段落纵向堆叠，无导航）。 */}
      <Tabs
        ariaLabel={t('settings.title')}
        value={tab}
        onChange={(id) => onTabChange(id as SettingsTab)}
        className="mb-5 w-full"
        items={[
          { id: 'connection', label: t('settings.section_connection'), icon: 'wifi' },
          { id: 'appearance', label: t('settings.section_appearance'), icon: 'palette' },
        ]}
      />

      {isConnection && (
        <div className="space-y-4">
          <Field
            label={t('settings.server_url')}
            hint={t('settings.server_hint')}
            required
            error={urlError ?? undefined}
          >
            <Input
              value={draft.serverUrl}
              leftIcon="server"
              placeholder="http://127.0.0.1:8080"
              invalid={!!urlError}
              onChange={(e) => setDraft({ ...draft, serverUrl: e.target.value })}
            />
          </Field>
          <Field label={t('settings.token')} hint={t('settings.token_hint')}>
            <Input
              type="password"
              value={draft.token}
              leftIcon="lock"
              placeholder={t('settings.token_placeholder')}
              onChange={(e) => setDraft({ ...draft, token: e.target.value })}
            />
          </Field>
          <div className="flex items-end gap-2">
            <Field label={t('settings.mode')} className="flex-1">
              <Select
                value={draft.mode}
                onChange={(e) => setDraft({ ...draft, mode: e.target.value as never })}
              >
                <option value="code">{t('settings.mode.code')}</option>
                <option value="architect">{t('settings.mode.architect')}</option>
                <option value="ask">{t('settings.mode.ask')}</option>
                <option value="debug">{t('settings.mode.debug')}</option>
              </Select>
            </Field>
            <Button variant="outline" leftIcon="wifi" loading={testing} onClick={testConnection}>
              {t('settings.test_connection')}
            </Button>
          </div>
          {/* SOCKS5 出站代理：仅影响后端发出的 HTTP/HTTPS 请求（LLM API 等），
              前端浏览器自身访问不经此代理。切换实时生效并由服务端持久化。 */}
          <div className="card flex items-center justify-between gap-3 p-3">
            <div className="min-w-0">
              <p className="text-sm font-medium text-text">{t('settings.socks5_title')}</p>
              <p className="mt-0.5 truncate text-xs text-muted">
                {socks5Status?.configured
                  ? (socks5Status.redacted ?? '')
                  : socks5LoadError
                    ? t('settings.socks5_load_failed')
                    : socks5Loading
                      ? t('settings.socks5_loading')
                      : t('settings.socks5_unconfigured')}
              </p>
            </div>
            {socks5LoadError && !socks5Status && (
              <Button
                variant="outline"
                size="sm"
                leftIcon="refresh"
                loading={socks5Loading}
                onClick={() => {
                  setSocks5Loading(true)
                  void refreshSocks5Status().then((ok) => {
                    setSocks5Loading(false)
                    setSocks5LoadError(!ok)
                  })
                }}
              >
                {t('common.retry')}
              </Button>
            )}
            {socks5Status?.configured && (
              <Switch
                checked={socks5Status.enabled}
                disabled={socks5Busy}
                label={t('settings.socks5_title')}
                onChange={(on) => {
                  setSocks5Busy(true)
                  void setSocks5Enabled(on).then((ok) => {
                    setSocks5Busy(false)
                    if (!ok) toast({ title: t('settings.socks5_fail'), severity: 'danger' })
                  })
                }}
              />
            )}
          </div>
        </div>
      )}

      {!isConnection && (
        <div className="space-y-5">
          <div>
            <SectionLabel icon="sun">{t('settings.theme')}</SectionLabel>
            <div className="mt-3 grid grid-cols-2 gap-3">
              <ThemePreview
                active={theme === 'light'}
                label={t('settings.light_theme')}
                onClick={() => setTheme('light')}
                variant="light"
              />
              <ThemePreview
                active={theme === 'dark'}
                label={t('settings.dark_theme')}
                onClick={() => setTheme('dark')}
                variant="dark"
              />
            </div>
          </div>

          <Divider />

          <Field label={t('settings.language')} hint={t('settings.language_hint')}>
            <Select
              value={preference}
              onChange={(e) => setPreference(e.target.value as LocaleCode | 'auto')}
            >
              <option value="auto">{t('lang.auto')}</option>
              {SUPPORTED_LOCALES.map((code) => (
                <option key={code} value={code}>
                  {t(`lang.${code}`)}
                </option>
              ))}
            </Select>
          </Field>

          <div>
            <SectionLabel icon="palette">{t('settings.accent')}</SectionLabel>
            <div className="mt-3 flex flex-wrap gap-2.5">
              {accents.map((a) => (
                <button
                  key={a.name}
                  type="button"
                  onClick={() => applyAccent(a.name)}
                  aria-pressed={accent === a.name}
                  title={a.name}
                  className={cn(
                    'focus-ring flex items-center gap-2 rounded-lg border py-1.5 pl-1.5 pr-3 text-xs transition-colors',
                    accent === a.name
                      ? 'border-primary bg-primary/10 text-text'
                      : 'border-border text-text-2 hover:border-border-strong hover:bg-surface-2',
                  )}
                >
                  <span
                    className="h-4 w-4 rounded-full"
                    style={{ background: `rgb(${resolveAccent(a, theme === 'dark').primary})` }}
                  />
                  {a.name}
                  {accent === a.name && <Icon name="check" size={14} className="text-primary" />}
                </button>
              ))}
            </div>
          </div>
        </div>
      )}
    </Modal>
  )
}

/** 主题预览卡：用真实令牌渲染缩略图，切换 accent 时同步（不再硬编码 teal）。 */
function ThemePreview({
  active,
  label,
  onClick,
  variant,
}: {
  active: boolean
  label: string
  onClick: () => void
  variant: 'light' | 'dark'
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={cn(
        'focus-ring overflow-hidden rounded-xl border-2 transition-colors',
        active ? 'border-primary' : 'border-border hover:border-border-strong',
      )}
    >
      <div
        className={cn(
          'flex h-16 items-end gap-1.5 p-3',
          variant === 'dark' ? 'bg-[#0b0d12]' : 'bg-[#f4f7fa]',
        )}
      >
        <div className={cn('h-8 w-2 rounded-full', variant === 'dark' ? 'bg-white/15' : 'bg-slate-300')} />
        <div
          className="h-7 w-full rounded-md"
          style={{
            background: 'linear-gradient(100deg, rgb(var(--c-primary)), rgb(var(--c-primary-glow)))',
          }}
        />
      </div>
      <div className="flex items-center justify-center gap-1.5 py-1.5 text-sm font-medium text-text">
        {active && <Icon name="check" size={14} className="text-primary" />}
        {label}
      </div>
    </button>
  )
}
