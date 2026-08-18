import type { InputHTMLAttributes, ReactNode } from 'react'

type Size = 'sm' | 'md' | 'lg'

export type InputProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'size'> & {
  size?: Size
  invalid?: boolean
  prefix?: ReactNode
  suffix?: ReactNode
}

export function Input({ size = 'md', invalid = false, prefix, suffix, className, ...rest }: InputProps) {
  const inputClass = ['ui-input', `size-${size}`, className ?? ''].filter(Boolean).join(' ')
  const input = <input {...rest} aria-invalid={invalid || undefined} className={inputClass} />
  if (prefix === undefined && suffix === undefined) return input
  return <span className="ui-input-wrap">{prefix}{input}{suffix}</span>
}
