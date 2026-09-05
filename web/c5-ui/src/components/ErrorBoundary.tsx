import { Component, type ErrorInfo, type ReactNode } from 'react'

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
              className="mt-4 rounded-lg bg-primary px-4 py-2 text-sm font-medium text-white transition-opacity hover:opacity-90"
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
