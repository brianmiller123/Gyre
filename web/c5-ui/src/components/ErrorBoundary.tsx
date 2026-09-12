import { Component, type ErrorInfo, type ReactNode, useState } from 'react'
import { useI18n } from '@/lib/i18n'

/**
 * 全局错误边界：捕获子树渲染期异常，显示可恢复卡片而非白屏。
 *
 * 挂在 Provider 栈之外（main.tsx），Provider 自身崩溃也能被兜住；
 * 文案不走 i18n——i18n Provider 崩溃时 t() 不可用。
 */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null }

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Unhandled render error:', error, info.componentStack)
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex min-h-screen items-center justify-center bg-surface p-6">
          <div className="w-full max-w-md rounded-2xl border border-danger/30 bg-surface-2 p-6 text-center shadow-soft">
            <h1 className="font-display text-lg font-semibold text-text">页面出错了 / Something went wrong</h1>
            <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-lg bg-surface p-3 text-left font-mono text-xs text-muted">
              {this.state.error.message}
            </pre>
            <button
              onClick={() => window.location.reload()}
              className="mt-4 rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-fg transition-opacity hover:opacity-90"
            >
              重新加载 / Reload
            </button>
          </div>
        </div>
      )
    }
    return this.props.children
  }
}

/** 面板级错误边界的类内核：捕获后渲染局部 fallback；重试 = 复位错误 + 通知外层重建子树。 */
class PanelBoundaryInner extends Component<
  { children: ReactNode; title: string; retryLabel: string; onRetry: () => void },
  { error: Error | null }
> {
  state = { error: null as Error | null }

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Panel render error:', error, info.componentStack)
  }

  render() {
    if (this.state.error) {
      return (
        <div
          role="alert"
          className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center"
        >
          <p className="text-sm font-medium text-danger">{this.props.title}</p>
          <pre className="max-h-24 max-w-full overflow-auto whitespace-pre-wrap break-words rounded-lg bg-surface-2 px-3 py-2 font-mono text-2xs text-muted">
            {this.state.error.message}
          </pre>
          <button
            onClick={() => {
              this.setState({ error: null })
              this.props.onRetry()
            }}
            className="rounded-lg border border-border bg-surface-2 px-3 py-1.5 text-xs font-medium text-text transition-colors hover:border-border-strong"
          >
            {this.props.retryLabel}
          </button>
        </div>
      )
    }
    return this.props.children
  }
}

/**
 * 面板级错误边界：单个面板（对话流/侧栏/运行面板/工作区）渲染崩溃只降级该面板，
 * 控制台其余部分继续可用。重试通过 bump key 就地重建子树（同时清理可能的脏状态），
 * 不丢其他面板的状态。必须挂在 Provider 栈之内（需要 i18n）。
 */
export function PanelBoundary({ label, children }: { label: string; children: ReactNode }) {
  const { t } = useI18n()
  const [attempt, setAttempt] = useState(0)
  return (
    <PanelBoundaryInner
      key={attempt}
      title={t('panel.error_title', { panel: label })}
      retryLabel={t('common.retry')}
      onRetry={() => setAttempt((a) => a + 1)}
    >
      {children}
    </PanelBoundaryInner>
  )
}
