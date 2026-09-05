/** Formatting helpers used across tables, KPI cards and charts. */

const compactFmt = new Intl.NumberFormat('en-US', {
  notation: 'compact',
  maximumFractionDigits: 1,
})
const numberFmt = new Intl.NumberFormat('en-US')

export function compact(n: number): string {
  return compactFmt.format(n)
}

export function formatNumber(n: number): string {
  return numberFmt.format(n)
}

export function currency(n: number): string {
  return '$' + numberFmt.format(Math.round(n))
}

export function percent(n: number, digits = 0): string {
  return `${n.toFixed(digits)}%`
}


/** Clamp a number into the [min, max] range. */
export function clamp(n: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, n))
}
