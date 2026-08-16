import type { HTMLAttributes, ReactNode } from 'react'

type Padding = 'sm' | 'md' | 'lg'

export type CardProps = HTMLAttributes<HTMLDivElement> & {
  padding?: Padding
  elevated?: boolean
  children?: ReactNode
}

export function Card({ padding = 'md', elevated = false, className, children, ...rest }: CardProps) {
  const classes = ['ui-card', `padding-${padding}`, elevated ? 'elevated' : '', className ?? ''].filter(Boolean).join(' ')
  return <div {...rest} className={classes}>{children}</div>
}
