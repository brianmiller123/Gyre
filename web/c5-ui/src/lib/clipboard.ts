/**
 * 剪贴板写入，兼容非安全上下文（`agent --serve` 常以 http://局域网IP 暴露，
 * 此时 `navigator.clipboard` 为 undefined，直接调用会静默失败）。
 *
 * 策略：安全上下文优先 Clipboard API；否则回退隐藏 textarea + execCommand。
 * 返回是否成功，调用方据此反馈（不得在失败时显示「已复制」）。
 * 必须在用户手势回调中调用（execCommand 依赖用户激活）。
 */
export async function copyText(text: string): Promise<boolean> {
  if (typeof navigator !== 'undefined' && navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text)
      return true
    } catch {
      /* 权限被拒或焦点丢失：落到 execCommand 回退 */
    }
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    const ok = document.execCommand('copy')
    document.body.removeChild(ta)
    return ok
  } catch {
    return false
  }
}
