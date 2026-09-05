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
import { useI18n } from '@/lib/i18n'

/**
 * Reusable UI primitives for the C5 console.
 * Everything is composable, theme-aware and accessible (ARIA roles, focus
 * rings, keyboard handling on the modal/dropdown/switch/checkbox).
 */

/* ------------------------------- tiny hooks ------------------------------- */
function useClickOutside<T extends HTMLElement>(
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
  const { t } = useI18n()
  const titleId = useId()
  const dialogRef = useRef<HTMLDivElement>(null)
  useLockBody(open)
  // 焦点管理：打开时移入对话框（键盘/读屏用户可直接 Tab 操作），关闭时归还给触发元素。
  useEffect(() => {
    if (!open) return
    const prev = document.activeElement instanceof HTMLElement ? document.activeElement : null
    dialogRef.current?.focus()
    return () => prev?.focus()
  }, [open])
  // 简易焦点陷阱：Tab 循环限制在对话框内，防止焦点落到背景内容上。
  const trapTab = (e: React.KeyboardEvent) => {
    if (e.key !== 'Tab') return
    const root = dialogRef.current
    if (!root) return
    const focusables = root.querySelectorAll<HTMLElement>(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    )
    if (focusables.length === 0) return
    const first = focusables[0]
    const last = focusables[focusables.length - 1]
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault()
      last.focus()
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault()
      first.focus()
    }
  }
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
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={title ? titleId : undefined}
        tabIndex={-1}
        onKeyDown={trapTab}
        className={cn(
          'relative z-10 flex max-h-[92vh] w-full flex-col overflow-hidden rounded-t-2xl border border-border bg-surface shadow-pop animate-scale-in outline-none sm:rounded-2xl',
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
                <h2 id={titleId} className="font-display text-base font-semibold text-text">{title}</h2>
              )}
              {description && <p className="mt-0.5 text-xs text-muted">{description}</p>}
            </div>
            <button
              onClick={onClose}
              className="-mr-1 flex h-8 w-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-text"
              aria-label={t('common.close')}
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
  direction = 'down',
  panelClassName,
}: {
  trigger: React.ReactElement
  items: MenuItem[]
  align?: 'left' | 'right'
  /** Panel placement relative to the trigger: `up` for bottom-anchored toolbars. */
  direction?: 'down' | 'up'
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
            'absolute z-50 min-w-[12rem] rounded-xl border border-border bg-surface p-1.5 shadow-pop animate-scale-in',
            direction === 'up' ? 'bottom-full mb-2 origin-bottom' : 'mt-2 origin-top',
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


export function Divider({ className }: { className?: string }) {
  return <hr className={cn('border-border', className)} />
}

