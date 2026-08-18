import type { InputHTMLAttributes, ReactNode } from 'react'

export type SwitchProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> & {
  label?: ReactNode
}

export function Switch({ label, className, ...rest }: SwitchProps) {
  const input = <input {...rest} type="checkbox" role="switch" />
  if (label === undefined) return input
  return (
    <label className={['ui-switch', className ?? ''].filter(Boolean).join(' ')}>
      {input}
      <span className="ui-checkbox-text">{label}</span>
    </label>
  )
}
