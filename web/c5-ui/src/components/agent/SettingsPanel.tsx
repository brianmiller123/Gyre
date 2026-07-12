import { useEffect, useState } from 'react'
import { Modal, Field, Input, Select, Button, Divider } from '@/components/ui'
import { Icon } from '@/components/icons'
import { useSettings } from '@/lib/settings'
import { useAgentSession } from '@/lib/agent/useAgentSession'
import { useTheme } from '@/lib/theme'
import { useNotifications } from '@/lib/notifications'
import { useI18n } from '@/lib/i18n'
import { SUPPORTED_LOCALES } from '@/lib/locales'
import type { LocaleCode } from '@/lib/locales'
import { cn } from '@/lib/cn'

const accents = [
  { name: 'Teal', primary: '13 148 136', glow: '45 212 191' },
  { name: 'Indigo', primary: '79 70 229', glow: '129 140 248' },
  { name: 'Blue', primary: '37 99 235', glow: '96 165 250' },
  { name: 'Emerald', primary: '5 150 105', glow: '52 211 153' },
  { name: 'Violet', primary: '124 58 237', glow: '167 139 250' },
  { name: 'Rose', primary: '225 29 72', glow: '251 113 133' },
  { name: 'Amber', primary: '217 119 6', glow: '251 191 36' },
]

/** Connection + appearance settings modal. */
export function SettingsPanel({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { settings, update } = useSettings()
  const { disconnect, connect, clear } = useAgentSession()
  const { theme, setTheme } = useTheme()
  const { toast } = useNotifications()
  const { t, preference, setPreference } = useI18n()

  const [draft, setDraft] = useState(settings)
  const [accent, setAccent] = useState('Teal')
  const [testing, setTesting] = useState(false)

  useEffect(() => {
    if (open) setDraft(settings)
  }, [open, settings])

  useEffect(() => {
    const saved = localStorage.getItem('agent-accent') || 'Teal'
    applyAccent(saved)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  function applyAccent(name: string) {
    const a = accents.find((x) => x.name === name) ?? accents[0]
    const root = document.documentElement
    root.style.setProperty('--c-primary', a.primary)
    root.style.setProperty('--c-primary-glow', a.glow)
    localStorage.setItem('agent-accent', name)
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
        body: t('settings.test_ok_body', { sessions: d.active_sessions ?? '?', models: d.models_available ?? '?' }),
        severity: 'success',
      })
    } catch (e) {
      toast({ title: t('settings.test_fail'), body: e instanceof Error ? e.message : String(e), severity: 'danger' })
    } finally {
      setTesting(false)
    }
  }

  function save() {
    update(draft)
    toast({ title: t('settings.saved'), severity: 'success' })
    disconnect()
    setTimeout(() => void connect(), 60)
    onClose()
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={t('settings.title')}
      description={t('settings.desc')}
      icon="settings"
      size="lg"
      footer={
        <>
          <Button variant="secondary" onClick={onClose}>
            {t('settings.cancel')}
          </Button>
          <Button variant="primary" leftIcon="check" onClick={save}>
            {t('settings.save_reconnect')}
          </Button>
        </>
      }
    >
      <div className="space-y-5">
        <div>
          <p className="mb-3 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-muted">
            <Icon name="wifi" size={13} /> {t('settings.section_connection')}
          </p>
          <div className="space-y-4">
            <Field
              label={t('settings.server_url')}
              hint={t('settings.server_hint')}
            >
              <Input
                value={draft.serverUrl}
                leftIcon="server"
                placeholder="http://127.0.0.1:8080"
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
                <Select value={draft.mode} onChange={(e) => setDraft({ ...draft, mode: e.target.value as any })}>
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
          </div>
        </div>

        <Divider />

        <div>
          <p className="mb-3 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-muted">
            <Icon name="palette" size={13} /> {t('settings.section_appearance')}
          </p>
          <div className="mb-4 grid grid-cols-2 gap-3">
            <ThemePreview active={theme === 'light'} label={t('settings.light_theme')} onClick={() => setTheme('light')} variant="light" />
            <ThemePreview active={theme === 'dark'} label={t('settings.dark_theme')} onClick={() => setTheme('dark')} variant="dark" />
          </div>
          <div className="mb-4">
            <Field label={t('settings.language')}>
              <Select
                value={preference}
                onChange={(e) => setPreference(e.target.value as LocaleCode | 'auto')}
              >
                <option value="auto">{t('lang.auto')}</option>
                {SUPPORTED_LOCALES.map((code) => (
                  <option key={code} value={code}>{t(`lang.${code}`)}</option>
                ))}
              </Select>
            </Field>
          </div>
          <p className="mb-2 text-sm font-medium text-text-2">{t('settings.accent')}</p>
          <div className="flex flex-wrap gap-3">
            {accents.map((a) => (
              <button
                key={a.name}
                onClick={() => applyAccent(a.name)}
                className={cn(
                  'flex flex-col items-center gap-1.5 rounded-xl border p-2.5 transition-all',
                  accent === a.name ? 'border-primary shadow-glow' : 'border-border hover:border-border-strong',
                )}
              >
                <span
                  className="h-8 w-8 rounded-full"
                  style={{ background: `rgb(${a.primary})`, boxShadow: accent === a.name ? `0 0 0 2px rgb(${a.glow})` : 'none' }}
                />
                <span className="text-[11px] text-text-2">{a.name}</span>
              </button>
            ))}
          </div>
        </div>

        <Divider />

        <div>
          <p className="mb-2 text-sm font-medium text-text-2">{t('settings.section_data')}</p>
          <Button
            variant="ghost"
            leftIcon="trash"
            className="text-danger hover:bg-danger/10"
            onClick={() => {
              clear()
              toast({ title: t('settings.clear_toast'), severity: 'info' })
            }}
          >
            {t('settings.clear_conversation')}
          </Button>
        </div>
      </div>
    </Modal>
  )
}

function ThemePreview({ active, label, onClick, variant }: { active: boolean; label: string; onClick: () => void; variant: 'light' | 'dark' }) {
  return (
    <button
      onClick={onClick}
      className={cn(
        'overflow-hidden rounded-xl border-2 transition-all',
        active ? 'border-primary shadow-glow' : 'border-border hover:border-border-strong',
      )}
    >
      <div className={cn('flex h-16 items-end gap-1.5 p-3', variant === 'dark' ? 'bg-[#0b0d12]' : 'bg-[#f4f7fa]')}>
        <div className={cn('h-8 w-2 rounded-full', variant === 'dark' ? 'bg-white/15' : 'bg-slate-300')} />
        <div className="h-7 w-full rounded-md" style={{ background: 'linear-gradient(100deg, rgb(13 148 136), rgb(45 212 191))' }} />
      </div>
      <div className="flex items-center justify-center gap-1.5 py-1.5 text-sm font-medium text-text">
        {active && <Icon name="check" size={13} className="text-primary" />}
        {label}
      </div>
    </button>
  )
}
