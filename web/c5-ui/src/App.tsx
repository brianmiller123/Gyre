import { ThemeProvider } from '@/lib/theme'
import { I18nProvider } from '@/lib/i18n'
import { SettingsProvider } from '@/lib/settings'
import { NotificationProvider } from '@/lib/notifications'
import { AgentSessionProvider } from '@/lib/agent/useAgentSession'
import { AgentShell } from '@/components/agent/AgentShell'

/**
 * Application root.
 *
 * Provider stack (outer → inner): i18n → theme → settings → notifications →
 * agent session. I18nProvider is outermost so every panel (including theme/
 * settings surfaces) can localize text. AgentSessionProvider reads connection
 * settings, and the shell + every panel consume the session, settings, theme
 * and i18n contexts.
 */
export default function App() {
  return (
    <I18nProvider>
      <ThemeProvider>
        <SettingsProvider>
          <NotificationProvider>
            <AgentSessionProvider>
              <AgentShell />
            </AgentSessionProvider>
          </NotificationProvider>
        </SettingsProvider>
      </ThemeProvider>
    </I18nProvider>
  )
}
