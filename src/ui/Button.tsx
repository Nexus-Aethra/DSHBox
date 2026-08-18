import type { ButtonHTMLAttributes } from 'react'

type Variant = 'primary' | 'secondary' | 'danger' | 'ghost'
type Size = 'sm' | 'md' | 'lg'
type Shape = 'default' | 'icon'

export type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant
  size?: Size
  shape?: Shape
  loading?: boolean
  block?: boolean
}

const variantClass: Record<Variant, string> = {
  primary: 'variant-primary',
  secondary: 'variant-secondary',
  danger: 'variant-danger',
  ghost: 'variant-ghost',
}

export function Button({
  variant = 'secondary',
  size = 'md',
  shape = 'default',
  loading = false,
  block = false,
  disabled,
  className,
  children,
  ...rest
}: ButtonProps) {
  const classes = ['ui-button', `size-${size}`, variantClass[variant], shape === 'icon' ? 'shape-icon' : '', block ? 'block' : '', className ?? ''].filter(Boolean).join(' ')
  return <button {...rest} disabled={disabled || loading} className={classes}>{loading ? <span className="ui-spinner" /> : null}{children}</button>
}
