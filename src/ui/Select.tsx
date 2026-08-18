import type { SelectHTMLAttributes } from 'react'

type Size = 'sm' | 'md' | 'lg'

export type SelectOption = { value: string; label: string }

export type SelectProps = Omit<SelectHTMLAttributes<HTMLSelectElement>, 'size'> & {
  size?: Size
  options: SelectOption[]
  placeholder?: string
}

export function Select({ size = 'md', options, placeholder, className, ...rest }: SelectProps) {
  const selectClass = ['ui-select', `size-${size}`, className ?? ''].filter(Boolean).join(' ')
  return (
    <select {...rest} className={selectClass}>
      {placeholder !== undefined && <option value="">{placeholder}</option>}
      {options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
    </select>
  )
}
