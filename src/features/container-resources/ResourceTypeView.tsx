import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { DshContainer, ResourceTypeSummary, ResourceView } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Select } from '../../ui/Select'

export type ResourceTypeText = {
  resourceTypeLoading: string
  resourceTypeContainers: string
  resourceTypeExtract: string
  resourceTypeStored: string
  resourceTypeEmpty: string
  resourceTypeSecret: string
  resourceTypeAbsent: string
  resourceTypeInjectTo: string
  resourceTypeInjectPlaceholder: string
  resourceTypeInject: string
  resourceTypeDelete: string
  resourceTypeRemove: string
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
 * Everything of one resource type: where each container stands, and what has
 * been extracted. Extract pulls a container's copy into the Box store; inject
 * pushes a stored copy into whichever container you pick.
 */
export function ResourceTypeView({ view, containers, text, removable = true, onRemove }: Props) {
  const [summary, setSummary] = useState<ResourceTypeSummary | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [busy, setBusy] = useState(false)
  const [queued, setQueued] = useState<string | null>(null)
  const [target, setTarget] = useState('')
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

  /** Run an action and refresh; anything that returns a task id is announced. */
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

  function human(bytes: number): string {
    const units = ['B', 'KB', 'MB', 'GB']
    let value = bytes
    let unit = 0
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1 }
    return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`
  }

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
      {loading && <p className="resource-note">{text.resourceTypeLoading}</p>}

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

      <h3 className="resource-section">{text.resourceTypeContainers}</h3>
      <table className="resource-table">
        <tbody>
          {(summary?.containers ?? []).map((entry) => (
            <tr key={entry.id}>
              <td>{entry.name}</td>
              <td className="resource-path">{entry.path}</td>
              <td>{entry.exists ? `${human(entry.bytes)} · ${text.resourceTypeFiles(entry.files)}` : text.resourceTypeAbsent}</td>
              <td>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={busy || !entry.exists}
                  onClick={() => { void run(() => boxApi.enqueueResourceExtract({ id: entry.id, kind: view.kind, ...(view.path !== null && view.path !== undefined && view.kind === 'path' ? { dest: view.path } : {}) })) }}
                >{text.resourceTypeExtract}</Button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3 className="resource-section">{text.resourceTypeStored}</h3>
      {(summary?.stored.length ?? 0) > 0 && (
        <div className="resource-from">
          <Select
            value={target}
            placeholder={text.resourceTypeInjectPlaceholder}
            aria-label={text.resourceTypeInjectTo}
            options={containers.map((entry) => ({ value: entry.id, label: entry.name }))}
            onChange={(event) => { setTarget(event.target.value) }}
          />
        </div>
      )}
      {(summary?.stored.length ?? 0) === 0 && <p className="resource-note">{text.resourceTypeEmpty}</p>}
      {(summary?.stored.length ?? 0) > 0 && (
        <>
          <table className="resource-table">
            <tbody>
              {summary?.stored.map((resource) => (
                <tr key={resource.id}>
                  <td>
                    {resource.id}
                    <div className="resource-browser-meta">{resource.sourcePath}</div>
                  </td>
                  <td>{human(resource.bytes)} · {text.resourceTypeFiles(resource.files)}</td>
                  <td>
                    {/* One inject per stored copy, into the container chosen
                        above: a type usually holds several. */}
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={busy || !target}
                      onClick={() => { void run(() => boxApi.enqueueResourceInject({ id: target, resource: resource.id, conflict, restart })) }}
                    >{text.resourceTypeInject}</Button>
                  </td>
                  <td>
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={busy}
                      onClick={() => { void run(() => boxApi.deleteResource(resource.id)) }}
                    >{text.resourceTypeDelete}</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  )
}
