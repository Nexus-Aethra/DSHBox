import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { DshContainer, ResourceTypeSummary, ResourceView } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Select } from '../../ui/Select'

export type ResourceTypeText = {
  resourceTypeLoading: string
  resourceTypeCopies: string
  resourceTypeCopiesEmpty: string
  resourceTypeAddCopy: string
  resourceTypeAddFrom: string
  resourceTypeAddFromPlaceholder: string
  resourceTypeFrom: string
  resourceTypeSecret: string
  resourceTypeAttach: string
  resourceTypeAttachPlaceholder: string
  resourceTypeDelete: string
  resourceTypeRemove: string
  resourceTypeOptions: string
  resourceTypeQueued: (id: string) => string
  resourceTypeError: (message: string) => string
  resourceTypeFiles: (count: number) => string
  resourceTypeRestart: string
  resourceTypeConflict: string
  resourceTypeMerge: string
  resourceTypeOverwrite: string
  resourceTypeRefuse: string
}

type Props = {
  view: ResourceView
  containers: DshContainer[]
  text: ResourceTypeText
  /** Built-in types ship with the navigation and cannot be removed. */
  removable?: boolean
  onRemove: () => Promise<void>
}

/**
 * A resource type is its list of extracted copies — nothing else. Above the
 * list you pick the container a fresh copy comes out of; on a copy you pick
 * the container it gets attached to. The conflict policy and the restart
 * switch are defaults, kept under 高级选项 so the list stays the page.
 */
export function ResourceTypeView({ view, containers, text, removable = true, onRemove }: Props) {
  const [summary, setSummary] = useState<ResourceTypeSummary | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [busy, setBusy] = useState(false)
  const [queued, setQueued] = useState<string | null>(null)
  const [source, setSource] = useState('')
  const [targets, setTargets] = useState<Record<string, string>>({})
  const [conflict, setConflict] = useState('merge')
  const [restart, setRestart] = useState(true)

  async function reload(): Promise<void> {
    setLoading(true)
    try {
      setSummary(await boxApi.listResourceType(view.kind, view.path ?? undefined))
      setError(null)
    } catch (loadError) {
      setError(String(loadError))
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => { void reload() }, [view.kind, view.path])

  /** Run an action and refresh; anything returning a task id is announced. */
  async function run(work: () => Promise<unknown>): Promise<void> {
    setBusy(true)
    setError(null)
    try {
      const result = await work()
      const id = (result as { id?: unknown } | null)?.id
      if (typeof id === 'string') setQueued(id)
      await reload()
    } catch (runError) {
      setError(String(runError))
    } finally {
      setBusy(false)
    }
  }

  /** Take a fresh copy out of the chosen container. */
  async function addCopy(): Promise<void> {
    if (!source) return
    await run(() => boxApi.enqueueResourceExtract({
      id: source,
      kind: view.kind,
      // A type pinned to a path takes exactly that path.
      ...(view.kind === 'path' && view.path !== null && view.path !== undefined ? { dest: view.path } : {}),
    }))
  }

  /** Put one copy into the container chosen on its row. */
  async function attach(copyId: string): Promise<void> {
    const target = targets[copyId]
    if (!target) return
    await run(() => boxApi.enqueueResourceInject({ id: target, resource: copyId, conflict, restart }))
  }

  function human(bytes: number): string {
    const units = ['B', 'KB', 'MB', 'GB']
    let value = bytes
    let unit = 0
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1 }
    return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`
  }

  function containerName(id: string): string {
    return containers.find((entry) => entry.id === id)?.name ?? id
  }

  const copies = summary?.stored ?? []

  return (
    <div className="resource-panel">
      <div className="resource-type-head">
        <h2>{view.label}</h2>
        <code className="resource-path">{view.path ?? view.kind}</code>
        {view.secret && <Badge variant="danger">{text.resourceTypeSecret}</Badge>}
        {removable && <Button variant="ghost" size="sm" disabled={busy} onClick={() => { void onRemove() }}>{text.resourceTypeRemove}</Button>}
      </div>

      {error !== null && <p className="resource-error">{text.resourceTypeError(error)}</p>}
      {queued !== null && <p className="resource-note">{text.resourceTypeQueued(queued)}</p>}

      <div className="resource-add">
        <span className="resource-section">{text.resourceTypeAddFrom}</span>
        <Select
          value={source}
          placeholder={text.resourceTypeAddFromPlaceholder}
          aria-label={text.resourceTypeAddFrom}
          options={containers.map((entry) => ({ value: entry.id, label: entry.name }))}
          onChange={(event) => { setSource(event.target.value) }}
        />
        <Button variant="primary" size="sm" disabled={busy || !source} onClick={() => { void addCopy() }}>{text.resourceTypeAddCopy}</Button>
        <details className="resource-options">
          <summary>{text.resourceTypeOptions}</summary>
          <div className="resource-controls">
            <Select
              value={conflict}
              aria-label={text.resourceTypeConflict}
              options={[
                { value: 'merge', label: text.resourceTypeMerge },
                { value: 'overwrite', label: text.resourceTypeOverwrite },
                { value: 'refuse', label: text.resourceTypeRefuse },
              ]}
              onChange={(event) => { setConflict(event.target.value) }}
            />
            <label className="resource-restart">
              <input type="checkbox" checked={restart} onChange={(event) => { setRestart(event.target.checked) }} />
              <span>{text.resourceTypeRestart}</span>
            </label>
          </div>
        </details>
      </div>

      <h3 className="resource-section">{text.resourceTypeCopies}</h3>
      {loading && <p className="resource-note">{text.resourceTypeLoading}</p>}
      {!loading && copies.length === 0 && <p className="resource-note">{text.resourceTypeCopiesEmpty}</p>}
      <ul className="resource-copies">
        {copies.map((copy) => (
          <li key={copy.id}>
            <div className="resource-copy-text">
              <div className="resource-copy-name">{copy.id}</div>
              <div className="resource-copy-meta">
                {human(copy.bytes)} · {text.resourceTypeFiles(copy.files)} · {text.resourceTypeFrom} {containerName(copy.sourceContainer)}
              </div>
            </div>
            <Select
              value={targets[copy.id] ?? ''}
              placeholder={text.resourceTypeAttachPlaceholder}
              aria-label={text.resourceTypeAttach}
              options={containers.map((entry) => ({ value: entry.id, label: entry.name }))}
              onChange={(event) => { setTargets((current) => ({ ...current, [copy.id]: event.target.value })) }}
            />
            <Button
              variant="secondary"
              size="sm"
              disabled={busy || !targets[copy.id]}
              onClick={() => { void attach(copy.id) }}
            >{text.resourceTypeAttach}</Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={busy}
              onClick={() => { void run(() => boxApi.deleteResource(copy.id)) }}
            >{text.resourceTypeDelete}</Button>
          </li>
        ))}
      </ul>
    </div>
  )
}
