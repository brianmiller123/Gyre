import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { cn } from '@/lib/cn'

/** 单序列数据点（label + 主/次值，次值用于双轴序列如 token/成本）。 */
export interface SeriesPoint {
  label: string
  value: number
  value2?: number
}

/**
 * Hand-rolled, dependency-free SVG charts.
 * They are responsive (measured width via ResizeObserver), theme-aware
 * (colors come from CSS variables) and animate in on mount.
 */

/* ------------------------------- shared hook ------------------------------ */
function useWidth<T extends HTMLElement>(fallback = 640) {
  const ref = useRef<T>(null)
  const [width, setWidth] = useState(fallback)
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const update = () => setWidth(el.clientWidth)
    update()
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  return [ref, width] as const
}



/* --------------------------------- BarChart ------------------------------- */
interface BarChartProps {
  data: SeriesPoint[]
  height?: number
  color?: string
  color2?: string
  label2?: string
  formatValue?: (v: number) => string
  className?: string
}

export function BarChart({
  data,
  height = 240,
  color = 'rgb(var(--c-primary))',
  color2 = 'rgb(var(--c-accent))',
  label2,
  formatValue = (v) => String(v),
  className,
}: BarChartProps) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<number | null>(null)
  const [mounted, setMounted] = useState(false)
  useEffect(() => {
    const t = requestAnimationFrame(() => setMounted(true))
    return () => cancelAnimationFrame(t)
  }, [])

  const padL = 44
  const padR = 12
  const padT = 14
  const padB = 28
  const innerW = Math.max(0, width - padL - padR)
  const innerH = height - padT - padB

  const has2 = data.some((d) => d.value2 != null)
  const max = Math.max(...data.flatMap((d) => [d.value, d.value2 ?? 0]), 1) * 1.12
  const max2 = Math.max(...data.map((d) => d.value2 ?? 0), 1) * 1.15

  const groupW = innerW / data.length
  const barGap = 2
  // 单柱限宽：数据点很少（如只有 1 天）时柱子不会涨满整格。
  const barW = Math.min(has2 ? groupW * 0.32 : groupW * 0.5, 32)

  const ticks = Array.from({ length: 5 }, (_, i) => (max / 4) * i)

  return (
    <div ref={ref} className={cn('relative w-full select-none', className)} style={{ height }}>
      <svg width={width} height={height} className="overflow-visible">
        {ticks.map((t, i) => {
          const y = padT + innerH - (t / max) * innerH
          return (
            <g key={i}>
              <line x1={padL} x2={width - padR} y1={y} y2={y} stroke="rgb(var(--c-border))" strokeWidth={1} strokeDasharray={i === 0 ? '0' : '3 4'} />
              <text x={padL - 10} y={y + 3} textAnchor="end" className="tabular fill-muted" fontSize="10">
                {formatValue(t)}
              </text>
            </g>
          )
        })}

        {data.map((d, i) => {
          const cx = padL + groupW * i + groupW / 2
          const h1 = mounted ? (d.value / max) * innerH : 0
          const h2 = mounted && has2 ? ((d.value2 ?? 0) / max2) * innerH * 0.6 : 0
          const active = hover === i
          return (
            <g
              key={i}
              onMouseEnter={() => setHover(i)}
              onMouseLeave={() => setHover(null)}
            >
              <rect
                x={cx - groupW / 2 + 4}
                y={padT}
                width={groupW - 8}
                height={innerH}
                fill={active ? 'rgb(var(--c-primary) / 0.08)' : 'transparent'}
                rx={6}
              />
              <rect
                x={cx - (has2 ? barW + barGap / 2 : barW / 2)}
                y={padT + innerH - h1}
                width={barW}
                height={h1}
                rx={5}
                fill={color}
                opacity={active ? 1 : 0.92}
                style={{ transition: 'height .7s cubic-bezier(.16,1,.3,1), y .7s cubic-bezier(.16,1,.3,1)' }}
              />
              {has2 && (
                <rect
                  x={cx - barW / 2 + barGap / 2}
                  y={padT + innerH - h2}
                  width={barW}
                  height={h2}
                  rx={4}
                  fill={color2}
                  style={{ transition: 'height .7s cubic-bezier(.16,1,.3,1), y .7s cubic-bezier(.16,1,.3,1)' }}
                />
              )}
              <text x={cx} y={height - 9} textAnchor="middle" className="fill-muted" fontSize="10">
                {d.label}
              </text>
            </g>
          )
        })}
      </svg>

      {hover !== null && (
        <div
          className="pointer-events-none absolute z-raised -translate-x-1/2 rounded-xl border border-border bg-surface/95 px-3 py-2 text-xs shadow-pop backdrop-blur"
          style={{ left: Math.min(Math.max(padL + groupW * hover + groupW / 2, 70), width - 70), top: 6 }}
        >
          <div className="mb-0.5 font-medium text-text">{data[hover].label}</div>
          <div className="tabular flex items-center gap-1.5 text-muted">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: color }} />
            {formatValue(data[hover].value)}
          </div>
          {has2 && data[hover].value2 != null && (
            <div className="tabular mt-0.5 flex items-center gap-1.5 text-muted">
              <span className="inline-block h-2 w-2 rounded-sm" style={{ background: color2 }} />
              {formatValue(data[hover].value2!)} {label2}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

