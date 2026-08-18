import type { HTMLAttributes, ReactNode } from 'react'

type Variant = 'neutral' | 'primary' | 'success' | 'danger'

export type BadgeProps = HTMLAttributes<HTMLSpanElement> & {
  variant?: Variant
  children?: ReactNode
}

export function Badge({ variant = 'neutral', className, children, ...rest }: BadgeProps) {
  const classes = ['ui-badge', variant !== 'neutral' ? `variant-${variant}` : '', className ?? ''].filter(Boolean).join(' ')
  return <span {...rest} className={classes}>{children}</span>
}
