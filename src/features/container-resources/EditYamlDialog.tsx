import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { ResourceTree } from '../../shared/types/domain'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'

export type EditYamlText = {
  resourceEdit: string
  resourceEditHint: string
  resourceEditPath: string
  resourceEditPathHint: string
  resourceEditBody: string
  resourceEditBodyHint: string
  resourceEditSave: string
  resourceEditSaving: string
  resourceEditTree: string
  resourceEditEmpty: string
  resourceEditQueued: (id: string) => string
  resourceEditError: (message: string) => string
  resourceConflict: string
  resourceConflictMerge: string
  resourceConflictOverwrite: string
  resourceConflictRefuse: string
  dialogCancel: string
}

type Props = {
  containerId: string
  path: string
  text: EditYamlText
  onClose: () => void
}

/**
 * Edit one YAML block of a container file, chosen by its key path.
 *
 * The document is shown as a tree because the block is the unit of change: a
 * provider route lives at `llm-pi-ai.providers.<route>`, and the rest of the
 * file belongs to other plugins. Picking a node loads that block; a path that
 * does not exist yet can be typed, which is how a block is added. Writing goes
 * through the same merge-or-replace policy an injected copy uses.
 */
export function EditYamlDialog({ containerId, path, text, onClose }: Props) {
  const [tree, setTree] = useState<ResourceTree | null>(null)
  const [section, setSection] = useState<string[]>([])
  const [body, setBody] = useState('')
  const [conflict, setConflict] = useState('merge')
  const [error, setError] = useState<string | null>(null)
  const [queued, setQueued] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function load(next: string[]): Promise<void> {
    setBusy(true)
    try {
      const loaded = await boxApi.readResourceTree({ id: containerId, path, section: next })
      setTree(loaded)
      setSection(next)
      setBody(loaded.text)
      setError(null)
    } catch (reason) {
      setError(String(reason))
    } finally {
      setBusy(false)
    }
  }

  useEffect(() => { void load([]) }, [containerId, path])

  async function save(): Promise<void> {
    setBusy(true)
    setError(null)
    try {
      const task = await boxApi.enqueueResourceWrite({ id: containerId, path, section, text: body, conflict })
      setQueued(task.id)
      const finished = await boxApi.waitForTask(task.id)
      if (finished !== null && finished.status === 'failed') {
        setError(finished.error ?? finished.stage)
      } else {
        await load(section)
      }
    } catch (reason) {
      setError(String(reason))
    } finally {
      setBusy(false)
    }
  }

  const pathText = section.join('.')

  return (
    <Dialog open title={text.resourceEdit} description={text.resourceEditHint} onClose={onClose} size="lg">
      <div className="resource-panel">
        <code className="resource-path">{path}</code>
        {error !== null && <p className="resource-error">{text.resourceEditError(error)}</p>}
        {queued !== null && <p className="resource-note">{text.resourceEditQueued(queued)}</p>}

        <span className="resource-section">{text.resourceEditTree}</span>
        {tree !== null && tree.nodes.length === 0 && <p className="resource-note">{text.resourceEditEmpty}</p>}
        <ul className="resource-yaml-tree">
          {(tree?.nodes ?? []).map((node) => {
            const selected = node.path.join('.') === pathText
            return (
              <li key={node.path.join('.') || 'root'}>
                <button
                  type="button"
                  className={selected ? 'active' : ''}
                  style={{ paddingLeft: `${8 + node.depth * 14}px` }}
                  onClick={() => { void load(node.path) }}
                >
                  <span className="resource-yaml-key">{node.key || '/'}</span>
                  <span className="resource-yaml-kind">{node.kind}</span>
                  <span className="resource-yaml-preview">{node.preview}</span>
                </button>
              </li>
            )
          })}
        </ul>

        <label className="resource-yaml-path">
          <span className="resource-section">{text.resourceEditPath}</span>
          <Input
            size="sm"
            value={pathText}
            placeholder={text.resourceEditPathHint}
            aria-label={text.resourceEditPath}
            onChange={(event) => { setSection(event.target.value.split('.').map((key) => key.trim()).filter(Boolean)) }}
            onKeyDown={(event) => { if (event.key === 'Enter') void load(section) }}
          />
        </label>
        <span className="resource-note">{text.resourceEditPathHint}</span>

        <span className="resource-section">{text.resourceEditBody}</span>
        <textarea
          className="resource-yaml-body"
          value={body}
          spellCheck={false}
          aria-label={text.resourceEditBody}
          placeholder={text.resourceEditBodyHint}
          onChange={(event) => { setBody(event.target.value) }}
        />

        <div className="resource-footer">
          <Select
            value={conflict}
            aria-label={text.resourceConflict}
            options={[
              { value: 'merge', label: text.resourceConflictMerge },
              { value: 'overwrite', label: text.resourceConflictOverwrite },
              { value: 'refuse', label: text.resourceConflictRefuse },
            ]}
            onChange={(event) => { setConflict(event.target.value) }}
          />
          <Button variant="ghost" size="sm" onClick={onClose}>{text.dialogCancel}</Button>
          <Button variant="primary" size="sm" disabled={busy || body.trim() === ''} onClick={() => { void save() }}>
            {busy ? text.resourceEditSaving : text.resourceEditSave}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
