import type { HTMLAttributes, ReactNode } from 'react'

type Gap = 2 | 3 | 4 | 5 | 6

export type StackProps = HTMLAttributes<HTMLDivElement> & {
  gap?: Gap
  children?: ReactNode
}

export function Stack({ gap = 4, className, children, ...rest }: StackProps) {
  const classes = ['ui-stack', `gap-${gap}`, className ?? ''].filter(Boolean).join(' ')
  return <div {...rest} className={classes}>{children}</div>
}

export type InlineProps = HTMLAttributes<HTMLDivElement> & {
  gap?: 2 | 3 | 4
  children?: ReactNode
}

export function Inline({ gap = 2, className, children, ...rest }: InlineProps) {
  const classes = ['ui-inline', `gap-${gap}`, className ?? ''].filter(Boolean).join(' ')
  return <div {...rest} className={classes}>{children}</div>
}
