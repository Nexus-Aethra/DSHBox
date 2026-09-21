import { useState } from 'react'
import type { DshContainer } from '../../shared/types/domain'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'

export type AddResourceCopyText = {
  resourceTypeAddCopy: string
  resourceTypeAddHint: string
  resourceTypeAddFrom: string
  resourceTypeAddFromPlaceholder: string
  resourceTypeCopyName: string
  resourceTypeCopyNameHint: string
  dialogCancel: string
}

type Props = {
  containers: DshContainer[]
  text: AddResourceCopyText
  onClose: () => void
  onAdd: (source: string, name: string) => Promise<void>
}

/**
 * Take a copy of a container's resource. Two things are asked for, because
 * neither has a good default: which container the state comes out of, and what
 * the copy is called — the name is what the list shows, and it is also what
 * lets the same container be copied twice without one copy replacing the other.
 */
export function AddResourceCopyDialog({ containers, text, onClose, onAdd }: Props) {
  const [source, setSource] = useState(containers[0]?.id ?? '')
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)

  async function add(): Promise<void> {
    if (!source) return
    setBusy(true)
    try {
      await onAdd(source, name.trim())
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open title={text.resourceTypeAddCopy} description={text.resourceTypeAddHint} onClose={onClose}>
      <div className="resource-panel">
        <Field label={text.resourceTypeAddFrom} required>
          {(fieldId) => (
            <Select
              id={fieldId}
              value={source}
              placeholder={text.resourceTypeAddFromPlaceholder}
              options={containers.map((entry) => ({ value: entry.id, label: entry.name }))}
              onChange={(event) => { setSource(event.target.value) }}
            />
          )}
        </Field>
        <Field label={text.resourceTypeCopyName} help={text.resourceTypeCopyNameHint}>
          {(fieldId) => (
            <Input
              id={fieldId}
              size="sm"
              value={name}
              placeholder={containers.find((entry) => entry.id === source)?.name ?? ''}
              onChange={(event) => { setName(event.target.value) }}
            />
          )}
        </Field>
        <div className="resource-footer">
          <Button variant="ghost" size="sm" onClick={onClose}>{text.dialogCancel}</Button>
          <Button variant="primary" size="sm" disabled={busy || !source} onClick={() => { void add() }}>
            {text.resourceTypeAddCopy}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
