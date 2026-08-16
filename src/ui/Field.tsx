import type { ReactNode } from 'react'
import { useId } from 'react'

export type FieldProps = {
  label: string
  help?: string
  error?: string
  required?: boolean
  htmlFor?: string
  children: (id: string) => ReactNode
}

export function Field({ label, help, error, required, htmlFor, children }: FieldProps) {
  const generated = useId()
  const id = htmlFor ?? generated
  return (
    <div className="ui-field">
      <label className="ui-field-label" htmlFor={id}>{label}{required ? <span className="ui-field-required">*</span> : null}</label>
      {children(id)}
      {error !== undefined && error !== '' ? <span className="ui-field-error">{error}</span> : help !== undefined && help !== '' ? <span className="ui-field-help">{help}</span> : null}
    </div>
  )
}
