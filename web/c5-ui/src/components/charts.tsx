import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useId } from 'react'
import type { SeriesPoint } from '@/types'
import { cn } from '@/lib/cn'

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

/** Catmull-Rom → cubic bézier smoothing for organic curves. */
function smoothPath(pts: Array<[number, number]>, tension = 1): string {
  if (pts.length < 2) return pts.length ? `M ${pts[0][0]},${pts[0][1]}` : ''
  let d = `M ${pts[0][0]},${pts[0][1]}`
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i]
    const p1 = pts[i]
    const p2 = pts[i + 1]
    const p3 = pts[i + 2] ?? p2
    const c1x = p1[0] + ((p2[0] - p0[0]) / 6) * tension
    const c1y = p1[1] + ((p2[1] - p0[1]) / 6) * tension
    const c2x = p2[0] - ((p3[0] - p1[0]) / 6) * tension
    const c2y = p2[1] - ((p3[1] - p1[1]) / 6) * tension
    d += ` C ${c1x},${c1y} ${c2x},${c2y} ${p2[0]},${p2[1]}`
  }
  return d
}

/* --------------------------------- Sparkline ------------------------------ */
export function Sparkline({
  data,
  color = 'rgb(var(--c-primary))',
  height = 36,
  strokeWidth = 2,
  fill = true,
  className,
}: {
  data: number[]
  color?: string
  height?: number
  strokeWidth?: number
  fill?: boolean
  className?: string
}) {
  const w = 120
  const h = height
  const max = Math.max(...data, 1)
  const min = Math.min(...data, 0)
  const range = max - min || 1
  const pts: Array<[number, number]> = data.map((v, i) => [
    (i / (data.length - 1)) * w,
    h - ((v - min) / range) * (h - 4) - 2,
  ])
  const line = smoothPath(pts, 0.8)
  const area = `${line} L ${w},${h} L 0,${h} Z`
  const id = useId().replace(/:/g, '')
  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      width="100%"
      height={h}
      preserveAspectRatio="none"
      className={className}
      aria-hidden="true"
    >
      <defs>
        <linearGradient id={`sp-${id}`} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={color} stopOpacity="0.28" />
          <stop offset="100%" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      {fill && <path d={area} fill={`url(#sp-${id})`} />}
      <path
        d={line}
        fill="none"
        stroke={color}
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  )
}

/* --------------------------------- AreaChart ------------------------------ */
interface AreaChartProps {
  data: SeriesPoint[]
  height?: number
  color?: string
  color2?: string
  label2?: string
  formatValue?: (v: number) => string
  className?: string
}

export function AreaChart({
  data,
  height = 260,
  color = 'rgb(var(--c-primary))',
  color2,
  formatValue = (v) => String(v),
  className,
}: AreaChartProps) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<number | null>(null)
  const id = useId().replace(/:/g, '')

  const padL = 44
  const padR = 14
  const padT = 16
  const padB = 26
  const innerW = Math.max(0, width - padL - padR)
  const innerH = height - padT - padB

  const all = data.flatMap((d) => [d.value, d.value2 ?? 0])
  const max = Math.max(...all, 1) * 1.12
  const min = 0
  const range = max - min || 1

  const xAt = (i: number) => padL + (data.length <= 1 ? 0 : (i / (data.length - 1)) * innerW)
  const yAt = (v: number) => padT + innerH - ((v - min) / range) * innerH

  const linePts: Array<[number, number]> = data.map((d, i) => [xAt(i), yAt(d.value)])
  const linePath = smoothPath(linePts, 0.6)
  const areaPath = `${linePath} L ${xAt(data.length - 1)},${padT + innerH} L ${xAt(0)},${padT + innerH} Z`

  const line2Pts: Array<[number, number]> | undefined = color2
    ? data.map((d, i) => [xAt(i), yAt(d.value2 ?? 0)])
    : undefined
  const line2Path = line2Pts ? smoothPath(line2Pts, 0.6) : undefined

  // y-axis gridlines (4 steps)
  const ticks = Array.from({ length: 5 }, (_, i) => (max / 4) * i)
  // x-axis labels: show ~6 evenly
  const labelStep = Math.max(1, Math.ceil(data.length / 6))

  function onMove(e: React.MouseEvent<HTMLDivElement>) {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    const ratio = (x - padL) / innerW
    const idx = Math.round(ratio * (data.length - 1))
    setHover(Math.min(data.length - 1, Math.max(0, idx)))
  }

  return (
    <div
      ref={ref}
      className={cn('relative w-full select-none', className)}
      style={{ height }}
      onMouseMove={onMove}
      onMouseLeave={() => setHover(null)}
    >
      <svg width={width} height={height} className="overflow-visible">
        <defs>
          <linearGradient id={`ar-${id}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={color} stopOpacity="0.32" />
            <stop offset="100%" stopColor={color} stopOpacity="0.02" />
          </linearGradient>
        </defs>

        {/* gridlines + y labels */}
        {ticks.map((t, i) => {
          const y = yAt(t)
          return (
            <g key={i}>
              <line
                x1={padL}
                x2={width - padR}
                y1={y}
                y2={y}
                stroke="rgb(var(--c-border))"
                strokeWidth={1}
                strokeDasharray={i === 0 ? '0' : '3 4'}
              />
              <text
                x={padL - 10}
                y={y + 3}
                textAnchor="end"
                className="tabular fill-muted"
                fontSize="10"
              >
                {formatValue(t)}
              </text>
            </g>
          )
        })}

        {/* area + line */}
        <path d={areaPath} fill={`url(#ar-${id})`} className="animate-fade-in" />
        <path
          d={linePath}
          fill="none"
          stroke={color}
          strokeWidth={2.5}
          strokeLinecap="round"
          strokeLinejoin="round"
          style={{ filter: 'drop-shadow(0 4px 10px rgb(var(--c-primary) / 0.25))' }}
        />
        {line2Path && (
          <path
            d={line2Path}
            fill="none"
            stroke={color2}
            strokeWidth={2}
            strokeDasharray="5 4"
            strokeLinecap="round"
          />
        )}

        {/* x labels */}
        {data.map((d, i) =>
          i % labelStep === 0 || i === data.length - 1 ? (
            <text
              key={i}
              x={xAt(i)}
              y={height - 8}
              textAnchor="middle"
              className="fill-muted"
              fontSize="10"
            >
              {d.label}
            </text>
          ) : null,
        )}

        {/* hover crosshair */}
        {hover !== null && (
          <g>
            <line
              x1={xAt(hover)}
              x2={xAt(hover)}
              y1={padT}
              y2={padT + innerH}
              stroke="rgb(var(--c-border-strong))"
              strokeWidth={1}
            />
            <circle cx={xAt(hover)} cy={yAt(data[hover].value)} r={4.5} fill={color} stroke="rgb(var(--c-surface))" strokeWidth={2} />
            {line2Path && data[hover].value2 != null && (
              <circle cx={xAt(hover)} cy={yAt(data[hover].value2!)} r={3.5} fill={color2} stroke="rgb(var(--c-surface))" strokeWidth={2} />
            )}
          </g>
        )}
      </svg>

      {hover !== null && (
        <div
          className="pointer-events-none absolute z-10 -translate-x-1/2 rounded-xl border border-border bg-surface/95 px-3 py-2 text-xs shadow-pop backdrop-blur"
          style={{
            left: Math.min(Math.max(xAt(hover), 70), width - 70),
            top: 6,
          }}
        >
          <div className="mb-0.5 font-medium text-text">{data[hover].label}</div>
          <div className="tabular flex items-center gap-1.5 text-muted">
            <span className="inline-block h-2 w-2 rounded-full" style={{ background: color }} />
            {formatValue(data[hover].value)}
          </div>
          {line2Path && data[hover].value2 != null && (
            <div className="tabular mt-0.5 flex items-center gap-1.5 text-muted">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: color2 }} />
              {formatValue(data[hover].value2!)}
            </div>
          )}
        </div>
      )}
    </div>
  )
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
  const barW = has2 ? groupW * 0.32 : groupW * 0.5

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
          className="pointer-events-none absolute z-10 -translate-x-1/2 rounded-xl border border-border bg-surface/95 px-3 py-2 text-xs shadow-pop backdrop-blur"
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

/* ----------------------------------- Donut -------------------------------- */
const DONUT_COLORS = [
  'rgb(var(--c-primary))',
  'rgb(var(--c-info))',
  'rgb(var(--c-accent))',
  'rgb(var(--c-success))',
  'rgb(var(--c-danger))',
  'rgb(168 85 247)',
]

export function Donut({
  data,
  size = 180,
  thickness = 18,
  centerLabel,
  centerSub,
}: {
  data: Array<{ label: string; value: number; color?: string }>
  size?: number
  thickness?: number
  centerLabel?: string
  centerSub?: string
}) {
  const total = data.reduce((s, d) => s + d.value, 0) || 1
  const r = (size - thickness) / 2
  const c = 2 * Math.PI * r
  let offset = 0

  return (
    <div className="relative inline-flex items-center justify-center" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" stroke="rgb(var(--c-surface-3))" strokeWidth={thickness} />
        {data.map((d, i) => {
          const len = (d.value / total) * c
          const seg = (
            <circle
              key={i}
              cx={size / 2}
              cy={size / 2}
              r={r}
              fill="none"
              stroke={d.color ?? DONUT_COLORS[i % DONUT_COLORS.length]}
              strokeWidth={thickness}
              strokeDasharray={`${len} ${c - len}`}
              strokeDashoffset={-offset}
              strokeLinecap="round"
              style={{ transition: 'stroke-dasharray .9s cubic-bezier(.16,1,.3,1)' }}
            />
          )
          offset += len
          return seg
        })}
      </svg>
      {(centerLabel || centerSub) && (
        <div className="absolute inset-0 flex flex-col items-center justify-center text-center">
          {centerLabel && (
            <span className="tabular font-display text-2xl font-bold text-text">{centerLabel}</span>
          )}
          {centerSub && <span className="mt-0.5 text-xs text-muted">{centerSub}</span>}
        </div>
      )}
    </div>
  )
}

/* -------------------------------- RadialGauge ----------------------------- */
export function RadialGauge({
  value,
  size = 96,
  thickness = 9,
  label,
  color,
}: {
  value: number
  size?: number
  thickness?: number
  label?: string
  color?: string
}) {
  const r = (size - thickness) / 2
  const c = 2 * Math.PI * r
  const pct = Math.min(100, Math.max(0, value))
  const tone =
    color ?? (pct >= 80 ? 'rgb(var(--c-danger))' : pct >= 60 ? 'rgb(var(--c-accent))' : 'rgb(var(--c-primary))')
  return (
    <div className="relative inline-flex items-center justify-center" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" stroke="rgb(var(--c-surface-3))" strokeWidth={thickness} />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={r}
          fill="none"
          stroke={tone}
          strokeWidth={thickness}
          strokeLinecap="round"
          strokeDasharray={`${(pct / 100) * c} ${c}`}
          style={{ transition: 'stroke-dasharray .9s cubic-bezier(.16,1,.3,1)' }}
        />
      </svg>
      <div className="absolute inset-0 flex flex-col items-center justify-center">
        <span className="tabular font-display text-base font-bold text-text">{Math.round(pct)}%</span>
        {label && <span className="text-[10px] text-muted">{label}</span>}
      </div>
    </div>
  )
}
