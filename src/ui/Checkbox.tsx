import type { InputHTMLAttributes, ReactNode } from 'react'

export type CheckboxProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> & {
  label?: ReactNode
}

export function Checkbox({ label, className, ...rest }: CheckboxProps) {
  const input = <input {...rest} type="checkbox" />
  if (label === undefined) return input
  return (
    <label className={['ui-checkbox', className ?? ''].filter(Boolean).join(' ')}>
      {input}
      <span className="ui-checkbox-text">{label}</span>
    </label>
  )
}
