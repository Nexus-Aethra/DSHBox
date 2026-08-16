import type { HTMLAttributes, ReactNode } from 'react'

export type ToolbarProps = HTMLAttributes<HTMLDivElement> & {
  justify?: 'start' | 'end' | 'between'
  children?: ReactNode
}

export function Toolbar({ justify, className, children, ...rest }: ToolbarProps) {
  const justifyClass = justify === 'end' ? 'justify-end' : justify === 'between' ? 'justify-between' : ''
  const classes = ['ui-toolbar', justifyClass, className ?? ''].filter(Boolean).join(' ')
  return <div {...rest} className={classes}>{children}</div>
}
