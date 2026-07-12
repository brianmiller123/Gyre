import {
  cloneElement,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from 'react'
import type { ReactNode } from 'react'
import { isValidElement } from 'react'
import { createPortal } from 'react-dom'
import { Icon } from '@/components/icons'
import { cn } from '@/lib/cn'
import { clamp } from '@/lib/format'

/**
 * Reusable UI primitives for the C5 console.
 * Everything is composable, theme-aware and accessible (ARIA roles, focus
 * rings, keyboard handling on the modal/dropdown/switch/checkbox).
 */

/* ------------------------------- tiny hooks ------------------------------- */
export function useClickOutside<T extends HTMLElement>(
  ref: React.RefObject<T>,
  handler: () => void,
) {
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) handler()
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [ref, handler])
}

function useLockBody(active: boolean) {
  useEffect(() => {
    if (!active) return
    const prev = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    return () => {
      document.body.style.overflow = prev
    }
  }, [active])
}

/* --------------------------------- Button --------------------------------- */
type ButtonVariant = 'primary' | 'secondary' | 'outline' | 'ghost' | 'danger'
type ButtonSize = 'sm' | 'md' | 'lg' | 'icon' | 'icon-sm'

const buttonVariants: Record<ButtonVariant, string> = {
  primary:
    'bg-primary text-white dark:text-[#06241f] hover:brightness-[1.07] shadow-sm shadow-primary/30',
  secondary: 'bg-surface-2 text-text border border-border hover:bg-surface-3',
  outline: 'border border-border-strong text-text-2 hover:bg-surface-2 hover:text-text',
  ghost: 'text-text-2 hover:bg-surface-2 hover:text-text',
  danger: 'bg-danger text-white hover:brightness-[1.07] shadow-sm shadow-danger/30',
}
const buttonSizes: Record<ButtonSize, string> = {
  sm: 'h-8 px-3 text-xs gap-1.5 rounded-lg',
  md: 'h-10 px-4 text-sm gap-2 rounded-lg',
  lg: 'h-11 px-5 text-sm gap-2 rounded-xl',
  icon: 'h-9 w-9 rounded-lg',
  'icon-sm': 'h-8 w-8 rounded-lg',
}

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant
  size?: ButtonSize
  loading?: boolean
  leftIcon?: string
  rightIcon?: string
}

export function Button({
  variant = 'secondary',
  size = 'md',
  loading = false,
  leftIcon,
  rightIcon,
  className,
  children,
  disabled,
  ...rest
}: ButtonProps) {
  return (
    <button
      className={cn(
        'inline-flex select-none items-center justify-center whitespace-nowrap font-medium transition-all duration-150 active:scale-[.97] disabled:pointer-events-none disabled:opacity-50 focus-visible:outline-none',
        buttonVariants[variant],
        buttonSizes[size],
        className,
      )}
      disabled={disabled || loading}
      {...rest}
    >
      {loading ? (
        <Spinner size={size === 'sm' ? 14 : 16} />
      ) : (
        leftIcon && <Icon name={leftIcon} size={size === 'sm' ? 15 : 17} />
      )}
      {children}
      {!loading && rightIcon && (
        <Icon name={rightIcon} size={size === 'sm' ? 15 : 17} />
      )}
    </button>
  )
}

/* ---------------------------------- Card ---------------------------------- */
export function Card({
  className,
  children,
  ...rest
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cn('card', className)} {...rest}>
      {children}
    </div>
  )
}

export function CardHeader({
  title,
  subtitle,
  icon,
  action,
  className,
}: {
  title: ReactNode
  subtitle?: ReactNode
  icon?: string
  action?: ReactNode
  className?: string
}) {
  return (
    <div
      className={cn(
        'flex items-start justify-between gap-4 border-b border-border px-5 py-4',
        className,
      )}
    >
      <div className="flex items-start gap-3">
        {icon && (
          <span className="mt-0.5 flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
            <Icon name={icon} size={18} />
          </span>
        )}
        <div>
          <h3 className="font-display text-[15px] font-semibold leading-tight text-text">
            {title}
          </h3>
          {subtitle && (
            <p className="mt-0.5 text-xs text-muted">{subtitle}</p>
          )}
        </div>
      </div>
      {action && <div className="shrink-0">{action}</div>}
    </div>
  )
}

/* --------------------------------- Badge ---------------------------------- */
export type Tone =
  | 'neutral'
  | 'primary'
  | 'success'
  | 'warning'
  | 'danger'
  | 'info'

const toneSoft: Record<Tone, string> = {
  neutral: 'bg-surface-3 text-text-2 ring-border',
  primary: 'bg-primary/12 text-primary',
  success: 'bg-success/14 text-success',
  warning: 'bg-warning/16 text-warning',
  danger: 'bg-danger/12 text-danger',
  info: 'bg-info/12 text-info',
}

const toneDot: Record<Tone, string> = {
  neutral: 'bg-muted',
  primary: 'bg-primary',
  success: 'bg-success',
  warning: 'bg-warning',
  danger: 'bg-danger',
  info: 'bg-info',
}

export function Badge({
  tone = 'neutral',
  dot = false,
  pulse = false,
  className,
  children,
}: {
  tone?: Tone
  dot?: boolean
  pulse?: boolean
  className?: string
  children: ReactNode
}) {
  return (
    <span
      className={cn(
        'inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium ring-1 ring-inset',
        toneSoft[tone],
        className,
      )}
    >
      {dot && (
        <span className="relative flex h-1.5 w-1.5">
          {pulse && (
            <span
              className={cn(
                'absolute inline-flex h-full w-full animate-ping rounded-full opacity-75',
                toneDot[tone],
              )}
            />
          )}
          <span
            className={cn('relative inline-flex h-1.5 w-1.5 rounded-full', toneDot[tone])}
          />
        </span>
      )}
      {children}
    </span>
  )
}

/* ------------------------------ status meta ------------------------------- */
// `label` 字段存 i18n key（见 lib/locales.ts），消费方用 useI18n().t(meta.label) 取本地化文案。
export const runStatusMeta: Record<string, { label: string; tone: Tone; pulse?: boolean }> = {
  running: { label: 'ui.status.running', tone: 'success', pulse: true },
  pending: { label: 'ui.status.pending', tone: 'warning', pulse: true },
  failed: { label: 'ui.status.failed', tone: 'danger' },
  stopped: { label: 'ui.status.stopped', tone: 'neutral' },
}
export const healthMeta: Record<string, { label: string; tone: Tone }> = {
  healthy: { label: 'ui.health.healthy', tone: 'success' },
  degraded: { label: 'ui.health.degraded', tone: 'warning' },
  down: { label: 'ui.health.down', tone: 'danger' },
}
export const deployMeta: Record<string, { label: string; tone: Tone; pulse?: boolean }> = {
  success: { label: 'ui.status.success', tone: 'success' },
  running: { label: 'ui.status.in_progress', tone: 'info', pulse: true },
  failed: { label: 'ui.status.failed', tone: 'danger' },
  cancelled: { label: 'ui.status.cancelled', tone: 'neutral' },
}
export const memberStatusMeta: Record<string, { label: string; tone: Tone }> = {
  active: { label: 'ui.member.active', tone: 'success' },
  invited: { label: 'ui.member.invited', tone: 'warning' },
  suspended: { label: 'ui.member.suspended', tone: 'neutral' },
}
export const providerMeta: Record<string, { label: string; tone: Tone }> = {
  aws: { label: 'AWS', tone: 'warning' },
  gcp: { label: 'GCP', tone: 'info' },
  azure: { label: 'Azure', tone: 'success' },
  onprem: { label: 'ui.provider.onprem', tone: 'neutral' },
}

/* --------------------------------- Field ---------------------------------- */
export function Field({
  label,
  hint,
  error,
  required,
  htmlFor,
  children,
  className,
}: {
  label?: ReactNode
  hint?: ReactNode
  error?: ReactNode
  required?: boolean
  htmlFor?: string
  children: ReactNode
  className?: string
}) {
  return (
    <div className={cn('space-y-1.5', className)}>
      {label && (
        <label
          htmlFor={htmlFor}
          className="flex items-center gap-1 text-sm font-medium text-text-2"
        >
          {label}
          {required && <span className="text-danger">*</span>}
        </label>
      )}
      {children}
      {error ? (
        <p className="flex items-center gap-1 text-xs font-medium text-danger">
          <Icon name="alert" size={12} />
          {error}
        </p>
      ) : hint ? (
        <p className="text-xs text-muted">{hint}</p>
      ) : null}
    </div>
  )
}

const inputBase =
  'w-full rounded-lg border bg-surface-2 text-sm text-text placeholder:text-muted/60 transition-colors focus:outline-none focus:ring-2 focus:ring-primary/25 disabled:opacity-50'

export interface InputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  invalid?: boolean
  leftIcon?: string
}

export function Input({
  className,
  invalid,
  leftIcon,
  ...rest
}: InputProps) {
  return (
    <div className="relative">
      {leftIcon && (
        <Icon
          name={leftIcon}
          size={16}
          className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted"
        />
      )}
      <input
        className={cn(
          inputBase,
          'h-10 px-3',
          leftIcon && 'pl-9',
          invalid
            ? 'border-danger focus:border-danger focus:ring-danger/20'
            : 'border-border focus:border-primary',
          className,
        )}
        {...rest}
      />
    </div>
  )
}

export function Textarea({
  className,
  invalid,
  ...rest
}: React.TextareaHTMLAttributes<HTMLTextAreaElement> & { invalid?: boolean }) {
  return (
    <textarea
      className={cn(
        inputBase,
        'min-h-[88px] resize-y px-3 py-2.5',
        invalid
          ? 'border-danger focus:border-danger focus:ring-danger/20'
          : 'border-border focus:border-primary',
        className,
      )}
      {...rest}
    />
  )
}

export function Select({
  className,
  invalid,
  children,
  ...rest
}: React.SelectHTMLAttributes<HTMLSelectElement> & { invalid?: boolean }) {
  return (
    <div className="relative">
      <select
        className={cn(
          inputBase,
          'h-10 appearance-none px-3 pr-9',
          invalid
            ? 'border-danger focus:border-danger focus:ring-danger/20'
            : 'border-border focus:border-primary',
          className,
        )}
        {...rest}
      >
        {children}
      </select>
      <Icon
        name="chevron-down"
        size={16}
        className="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 text-muted"
      />
    </div>
  )
}

/* -------------------------------- Switch ---------------------------------- */
export function Switch({
  checked,
  onChange,
  disabled,
  id,
  label,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  disabled?: boolean
  id?: string
  label?: string
}) {
  return (
    <button
      type="button"
      role="switch"
      id={id}
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cn(
        'relative inline-flex h-6 w-11 shrink-0 items-center rounded-full transition-colors disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/40',
        checked ? 'bg-primary' : 'border border-border bg-surface-3',
      )}
    >
      <span
        className={cn(
          'inline-block h-5 w-5 transform rounded-full bg-white shadow transition-transform',
          checked ? 'translate-x-[22px]' : 'translate-x-0.5',
        )}
      />
    </button>
  )
}

export function Checkbox({
  checked,
  onChange,
  indeterminate = false,
  'aria-label': ariaLabel,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  indeterminate?: boolean
  'aria-label'?: string
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={indeterminate ? 'mixed' : checked}
      aria-label={ariaLabel}
      onClick={() => onChange(!checked)}
      className={cn(
        'flex h-[18px] w-[18px] shrink-0 items-center justify-center rounded-[6px] border transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/40',
        checked || indeterminate
          ? 'border-primary bg-primary'
          : 'border-border-strong bg-surface-2 hover:border-primary/60',
      )}
    >
      <Icon
        name={indeterminate ? 'minus' : 'check'}
        size={13}
        className="text-white dark:text-[#06241f]"
        strokeWidth={3}
      />
    </button>
  )
}

/* --------------------------------- Avatar --------------------------------- */
export function Avatar({
  name,
  hue = 200,
  size = 36,
  className,
}: {
  name: string
  hue?: number
  size?: number
  className?: string
}) {
  return (
    <span
      className={cn(
        'inline-flex shrink-0 items-center justify-center rounded-full font-semibold text-white',
        className,
      )}
      style={{
        width: size,
        height: size,
        fontSize: size * 0.4,
        background: `linear-gradient(135deg, hsl(${hue} 68% 46%), hsl(${(hue + 42) % 360} 70% 38%))`,
      }}
      title={name}
    >
      {name.trim().slice(0, 1)}
    </span>
  )
}

/* ------------------------------- ProgressBar ------------------------------ */
export function ProgressBar({
  value,
  tone = 'primary',
  size = 'md',
  className,
}: {
  value: number
  tone?: Tone
  size?: 'sm' | 'md'
  className?: string
}) {
  const fill: Record<Tone, string> = {
    neutral: 'bg-muted',
    primary: 'bg-primary',
    success: 'bg-success',
    warning: 'bg-warning',
    danger: 'bg-danger',
    info: 'bg-info',
  }
  return (
    <div
      className={cn(
        'w-full overflow-hidden rounded-full bg-surface-3',
        size === 'sm' ? 'h-1.5' : 'h-2.5',
        className,
      )}
    >
      <div
        className={cn('h-full rounded-full transition-[width] duration-700 ease-out', fill[tone])}
        style={{ width: `${clamp(value, 0, 100)}%` }}
      />
    </div>
  )
}

/* -------------------------------- Segmented ------------------------------- */
export function Segmented<T extends string>({
  options,
  value,
  onChange,
  className,
}: {
  options: Array<{ value: T; label: string; icon?: string }>
  value: T
  onChange: (v: T) => void
  className?: string
}) {
  return (
    <div
      className={cn(
        'inline-flex items-center gap-1 rounded-xl border border-border bg-surface-2 p-1',
        className,
      )}
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            'inline-flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium transition-all',
            value === o.value
              ? 'bg-surface text-text shadow-soft'
              : 'text-muted hover:text-text',
          )}
        >
          {o.icon && <Icon name={o.icon} size={15} />}
          {o.label}
        </button>
      ))}
    </div>
  )
}

/* ---------------------------------- Modal --------------------------------- */
const modalSizes = {
  sm: 'max-w-md',
  md: 'max-w-lg',
  lg: 'max-w-2xl',
  xl: 'max-w-4xl',
}

export function Modal({
  open,
  onClose,
  title,
  description,
  icon,
  children,
  footer,
  size = 'md',
}: {
  open: boolean
  onClose: () => void
  title?: ReactNode
  description?: ReactNode
  icon?: string
  children: ReactNode
  footer?: ReactNode
  size?: keyof typeof modalSizes
}) {
  useLockBody(open)
  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, onClose])

  if (!open) return null
  return createPortal(
    <div className="fixed inset-0 z-[100] flex items-end justify-center p-0 sm:items-center sm:p-4">
      <div
        className="absolute inset-0 bg-black/55 backdrop-blur-sm animate-fade-in"
        onClick={onClose}
      />
      <div
        role="dialog"
        aria-modal="true"
        className={cn(
          'relative z-10 flex max-h-[92vh] w-full flex-col overflow-hidden rounded-t-2xl border border-border bg-surface shadow-pop animate-scale-in sm:rounded-2xl',
          modalSizes[size],
        )}
      >
        {(title || icon) && (
          <div className="flex items-start gap-3 border-b border-border px-5 py-4">
            {icon && (
              <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
                <Icon name={icon} size={18} />
              </span>
            )}
            <div className="min-w-0 flex-1">
              {title && (
                <h2 className="font-display text-base font-semibold text-text">{title}</h2>
              )}
              {description && <p className="mt-0.5 text-xs text-muted">{description}</p>}
            </div>
            <button
              onClick={onClose}
              className="-mr-1 flex h-8 w-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-text"
              aria-label="关闭"
            >
              <Icon name="close" size={18} />
            </button>
          </div>
        )}
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
        {footer && (
          <div className="flex items-center justify-end gap-2 border-t border-border bg-surface-2/60 px-5 py-3">
            {footer}
          </div>
        )}
      </div>
    </div>,
    document.body,
  )
}

/* -------------------------------- Dropdown -------------------------------- */
export interface MenuItem {
  label?: string
  icon?: string
  onClick?: () => void
  danger?: boolean
  active?: boolean
  disabled?: boolean
  divider?: boolean
}

export function Dropdown({
  trigger,
  items,
  align = 'right',
  panelClassName,
}: {
  trigger: React.ReactElement
  items: MenuItem[]
  align?: 'left' | 'right'
  panelClassName?: string
}) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLSpanElement>(null)
  const close = useCallback(() => setOpen(false), [])
  useClickOutside(ref, close)
  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && close()
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, close])

  const triggerEl = isValidElement(trigger)
    ? cloneElement(trigger, {
        onClick: (e: React.MouseEvent) => {
          ;(trigger.props as { onClick?: (e: React.MouseEvent) => void }).onClick?.(e)
          setOpen((o) => !o)
        },
      } as Record<string, unknown>)
    : trigger

  return (
    <span className="relative inline-flex" ref={ref}>
      {triggerEl}
      {open && (
        <div
          role="menu"
          className={cn(
            'absolute z-50 mt-2 min-w-[12rem] origin-top rounded-xl border border-border bg-surface p-1.5 shadow-pop animate-scale-in',
            align === 'right' ? 'right-0' : 'left-0',
            panelClassName,
          )}
        >
          {items.map((it, i) =>
            it.divider ? (
              <div key={i} className="my-1 h-px bg-border" />
            ) : (
              <button
                key={i}
                role="menuitem"
                disabled={it.disabled}
                onClick={() => {
                  it.onClick?.()
                  close()
                }}
                className={cn(
                  'flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-40',
                  it.danger
                    ? 'text-danger hover:bg-danger/10'
                    : 'text-text-2 hover:bg-surface-2 hover:text-text',
                  it.active && 'bg-surface-2 text-text',
                )}
              >
                {it.icon && <Icon name={it.icon} size={16} className="shrink-0" />}
                <span className="flex-1 truncate">{it.label}</span>
                {it.active && <Icon name="check" size={15} className="text-primary" />}
              </button>
            ),
          )}
        </div>
      )}
    </span>
  )
}

/* ------------------------------ misc primitives --------------------------- */
export function Spinner({ size = 16, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={cn('animate-spin', className)}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden="true"
    >
      <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="3" opacity="0.22" />
      <path
        d="M21 12a9 9 0 0 0-9-9"
        stroke="currentColor"
        strokeWidth="3"
        strokeLinecap="round"
      />
    </svg>
  )
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn('skeleton', className)} />
}

export function EmptyState({
  icon = 'inbox',
  title,
  description,
  action,
  className,
}: {
  icon?: string
  title: string
  description?: string
  action?: ReactNode
  className?: string
}) {
  return (
    <div
      className={cn(
        'flex flex-col items-center justify-center px-6 py-14 text-center',
        className,
      )}
    >
      <span className="mb-4 flex h-14 w-14 items-center justify-center rounded-2xl bg-surface-2 text-muted">
        <Icon name={icon} size={26} />
      </span>
      <h3 className="font-display text-base font-semibold text-text">{title}</h3>
      {description && (
        <p className="mt-1 max-w-sm text-sm text-muted">{description}</p>
      )}
      {action && <div className="mt-5">{action}</div>}
    </div>
  )
}

export function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="inline-flex h-5 min-w-[1.25rem] items-center justify-center rounded border border-border bg-surface-2 px-1.5 font-mono text-[11px] text-muted">
      {children}
    </kbd>
  )
}

export function Tooltip({
  label,
  children,
  side = 'top',
}: {
  label: string
  children: ReactNode
  side?: 'top' | 'bottom'
}) {
  return (
    <span className="group/tt relative inline-flex">
      {children}
      <span
        className={cn(
          'pointer-events-none absolute left-1/2 z-50 -translate-x-1/2 whitespace-nowrap rounded-md border border-border bg-surface px-2 py-1 text-xs text-text shadow-pop transition-all duration-150 opacity-0 group-hover/tt:opacity-100',
          side === 'top' ? 'bottom-full mb-1.5' : 'top-full mt-1.5',
        )}
      >
        {label}
      </span>
    </span>
  )
}

export function Divider({ className }: { className?: string }) {
  return <hr className={cn('border-border', className)} />
}

export function SegmentedTabs<T extends string>({
  tabs,
  value,
  onChange,
  className,
}: {
  tabs: Array<{ value: T; label: string; count?: number }>
  value: T
  onChange: (v: T) => void
  className?: string
}) {
  return (
    <div className={cn('flex items-center gap-1 overflow-x-auto no-scrollbar', className)}>
      {tabs.map((t) => (
        <button
          key={t.value}
          onClick={() => onChange(t.value)}
          className={cn(
            'relative whitespace-nowrap rounded-lg px-3 py-2 text-sm font-medium transition-colors',
            value === t.value ? 'text-primary' : 'text-muted hover:text-text',
          )}
        >
          {t.label}
          {typeof t.count === 'number' && (
            <span
              className={cn(
                'ml-1.5 rounded-full px-1.5 py-0.5 text-[10px] font-semibold tabular',
                value === t.value ? 'bg-primary/15 text-primary' : 'bg-surface-3 text-muted',
              )}
            >
              {t.count}
            </span>
          )}
          {value === t.value && (
            <span className="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-primary" />
          )}
        </button>
      ))}
    </div>
  )
}

// re-export useId consumers can import if needed
export { useId }
