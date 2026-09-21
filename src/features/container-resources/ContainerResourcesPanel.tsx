import { useEffect, useMemo, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { EditYamlText } from './EditYamlDialog'
import { EditYamlDialog } from './EditYamlDialog'
import { ContainerTree, PluginChips, useInstalledPlugins } from './pickers'
import type { PickerText } from './pickers'
import type { ContainerResources, DiscoveredResource, DshContainer, StoredResource } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'

export type ResourceText = PickerText & EditYamlText & {
  resourceOpen: string
  resourceTitle: (name: string) => string
  resourceSubtitle: string
  resourceDiscovered: string
  resourceStored: string
  resourceStoredEmpty: string
  resourceKind: string
  resourcePath: string
  resourceSource: string
  resourceSize: string
  resourceState: string
  resourceAbsent: string
  resourceSecret: string
  resourceBuiltin: string
  resourceDeclared: string
  resourceInferred: string
  resourceExtract: string
  resourceInject: string
  resourceDelete: string
  resourceEntry: string
  resourceEntryPlaceholder: string
  resourceConflict: string
  resourceConflictMerge: string
  resourceConflictOverwrite: string
  resourceConflictRefuse: string
  resourceRestart: string
  resourcePlugin: string
  resourcePluginPlaceholder: string
  resourceDetect: string
  resourceBrowse: string
  resourceBrowseSelected: (path: string) => string
  resourceBrowseExtract: string
  resourceBrowseInject: string
  resourceEdit: string
  resourceBrowseInjectPick: string
  resourceFrom: string
  resourceFromPlaceholder: string
  resourceInjectFrom: string
  resourceQueued: (id: string) => string
  resourceEmpty: string
  resourceLoading: string
  resourceError: (message: string) => string
  resourceClose: string
}

type Props = {
  id: string
  name: string
  text: ResourceText
  onClose: () => void
}

/**
 * What a container persists, and how to move it: extract a kind (or one entry,
 * e.g. a single conversation) into the Box store, inject it back, or carry a
 * resource in from another container. Every action is a daemon task, so it
 * shows up under Tasks with progress and a log.
 */
export function ContainerResourcesPanel({ id, name, text, onClose }: Props) {
  const [data, setData] = useState<ContainerResources | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [plugin, setPlugin] = useState('')
  const [browseOpen, setBrowseOpen] = useState(false)
  const [pluginQuery, setPluginQuery] = useState('')
  const [picked, setPicked] = useState<string | null>(null)
  const [editOpen, setEditOpen] = useState(false)
  const [injectPick, setInjectPick] = useState('')
  const [entry, setEntry] = useState('')
  const [conflict, setConflict] = useState('merge')
  const [restart, setRestart] = useState(true)
  const [from, setFrom] = useState('')
  const [fromKind, setFromKind] = useState('sessions')
  const [containers, setContainers] = useState<DshContainer[]>([])
  const [busy, setBusy] = useState(false)
  const [queued, setQueued] = useState<string | null>(null)

  async function reload(nextPlugin = plugin): Promise<void> {
    setLoading(true)
    try {
      setData(await boxApi.listContainerResources(id, nextPlugin || undefined))
      setError(null)
    } catch (loadError) {
      setError(String(loadError))
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    void reload('')
    boxApi.listContainers().then(setContainers).catch(() => { setContainers([]) })
    // Reloading on container change is the whole point; `plugin` is applied by
    // the Detect button so a half-typed package name never hits the daemon.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  /** Extract exactly the path picked in the tree, named after its last segment. */
  async function extractPickedPath(): Promise<void> {
    if (picked === null) return
    const name = picked.split('/').filter(Boolean).pop() ?? picked
    await run(() => boxApi.enqueueResourceExtract({ id, kind: 'path', dest: picked, name }))
  }

  /** Put a stored resource at the picked path. */
  async function injectIntoPickedPath(): Promise<void> {
    if (picked === null || !injectPick) return
    await run(() => boxApi.enqueueResourceInject({ id, resource: injectPick, dest: picked, conflict, restart }))
  }

  const installedPlugins = useInstalledPlugins(id)
  const others = useMemo(() => containers.filter((container) => container.id !== id), [containers, id])

  /** Queue an action, wait for the daemon to finish it, then refresh. */
  async function run(work: () => Promise<{ id: string }>): Promise<void> {
    setBusy(true)
    setError(null)
    try {
      const task = await work()
      setQueued(task.id ?? '')
      const finished = await boxApi.waitForTask(task.id)
      if (finished !== null && finished.status === 'failed') setError(finished.error ?? finished.stage)
      await reload()
    } catch (runError) {
      setError(String(runError))
    } finally {
      setBusy(false)
    }
  }

  async function extract(resource: DiscoveredResource): Promise<void> {
    await run(() => boxApi.enqueueResourceExtract({
      id,
      kind: resource.kind.id,
      entry: entry.trim() || undefined,
      plugin: resource.plugin ?? (plugin.trim() || undefined),
      dest: resource.scope === 'inferred' ? resource.kind.path : undefined,
    }))
  }

  async function inject(resource: StoredResource): Promise<void> {
    await run(() => boxApi.enqueueResourceInject({
      id,
      resource: resource.id,
      conflict,
      restart,
    }))
  }

  async function injectFromContainer(): Promise<void> {
    if (!from) return
    await run(() => boxApi.enqueueResourceInject({
      id,
      from,
      kind: fromKind.trim() || 'sessions',
      conflict,
      restart,
    }))
  }

  async function remove(resource: StoredResource): Promise<void> {
    setBusy(true)
    try {
      await boxApi.deleteResource(resource.id)
      await reload()
    } catch (removeError) {
      setError(String(removeError))
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

  function scopeLabel(scope: DiscoveredResource['scope']): string {
    if (scope === 'declared') return text.resourceDeclared
    if (scope === 'inferred') return text.resourceInferred
    return text.resourceBuiltin
  }

  return (
    <Dialog open title={text.resourceTitle(name)} description={text.resourceSubtitle} onClose={onClose} size="lg">
      <div className="resource-panel">
        {error !== null && <p className="resource-error">{text.resourceError(error)}</p>}
        {queued !== null && <p className="resource-note">{text.resourceQueued(queued)}</p>}

        <div className="resource-controls">
          <Input
            size="sm"
            value={entry}
            placeholder={text.resourceEntryPlaceholder}
            aria-label={text.resourceEntry}
            onChange={(event) => { setEntry(event.target.value) }}
          />
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
          <label className="resource-restart">
            <input type="checkbox" checked={restart} onChange={(event) => { setRestart(event.target.checked) }} />
            <span>{text.resourceRestart}</span>
          </label>
        </div>

        <PluginChips
          plugins={installedPlugins}
          query={pluginQuery}
          onQuery={setPluginQuery}
          selected={plugin}
          onSelect={(name) => { setPlugin(name); void reload(name) }}
          text={text}
        />
        <div className="resource-detect">
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => { void reload(plugin) }}>{text.resourceDetect}</Button>
          <Button variant="ghost" size="sm" onClick={() => { setBrowseOpen((open) => !open) }}>{text.resourceBrowse}</Button>
        </div>

        {browseOpen && (
          <ContainerTree id={id} text={text} onPick={(path) => { setPicked(path) }} />
        )}
        {picked !== null && (
          <div className="resource-browser-pick">
            <span className="resource-note">{text.resourceBrowseSelected(picked)}</span>
            <Button variant="secondary" size="sm" disabled={busy} onClick={() => { void extractPickedPath() }}>{text.resourceBrowseExtract}</Button>
            <Select
              value={injectPick}
              placeholder={text.resourceBrowseInjectPick}
              aria-label={text.resourceBrowseInject}
              options={(data?.stored ?? []).map((resource) => ({ value: resource.id, label: resource.id }))}
              onChange={(event) => { setInjectPick(event.target.value) }}
            />
            <Button variant="secondary" size="sm" disabled={busy || !injectPick} onClick={() => { void injectIntoPickedPath() }}>{text.resourceBrowseInject}</Button>
            <Button variant="secondary" size="sm" disabled={busy} onClick={() => { setEditOpen(true) }}>{text.resourceEdit}</Button>
          </div>
        )}

        <h3 className="resource-section">{text.resourceDiscovered}</h3>
        {loading && <p className="resource-note">{text.resourceLoading}</p>}
        {!loading && (data?.resources.length ?? 0) === 0 && <p className="resource-note">{text.resourceEmpty}</p>}
        {!loading && (data?.resources.length ?? 0) > 0 && (
          <table className="resource-table">
            <thead>
              <tr>
                <th>{text.resourceKind}</th>
                <th>{text.resourcePath}</th>
                <th>{text.resourceSource}</th>
                <th>{text.resourceSize}</th>
                <th>{text.resourceState}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data?.resources.map((resource) => (
                <tr key={`${resource.kind.id}:${resource.kind.path}`}>
                  <td>
                    {resource.kind.label}
                    {resource.kind.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
                  </td>
                  <td className="resource-path">{resource.kind.path}</td>
                  <td>{scopeLabel(resource.scope)}</td>
                  <td>{resource.exists ? human(resource.bytes) : text.resourceAbsent}</td>
                  <td>{resource.exists ? `${resource.files}` : '—'}</td>
                  <td>
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={busy || !resource.exists}
                      onClick={() => { void extract(resource) }}
                    >{text.resourceExtract}</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <h3 className="resource-section">{text.resourceStored}</h3>
        {(data?.stored.length ?? 0) === 0 && <p className="resource-note">{text.resourceStoredEmpty}</p>}
        {(data?.stored.length ?? 0) > 0 && (
          <table className="resource-table">
            <tbody>
              {data?.stored.map((resource) => (
                <tr key={resource.id}>
                  <td>
                    {resource.id}
                    {resource.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
                    <div className="resource-browser-meta">{resource.sourcePath}</div>
                  </td>
                  <td>{human(resource.bytes)} · {resource.files}</td>
                  <td>
                    <Button variant="secondary" size="sm" disabled={busy} onClick={() => { void inject(resource) }}>{text.resourceInject}</Button>
                  </td>
                  <td>
                    <Button variant="ghost" size="sm" disabled={busy} onClick={() => { void remove(resource) }}>{text.resourceDelete}</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        <div className="resource-from">
          <Field label={text.resourceFrom}>
            {(fieldId) => (
              <Select
                id={fieldId}
                value={from}
                placeholder={text.resourceFromPlaceholder}
                options={others.map((container) => ({ value: container.id, label: container.name }))}
                onChange={(event) => { setFrom(event.target.value) }}
              />
            )}
          </Field>
          <Input
            size="sm"
            value={fromKind}
            aria-label={text.resourceKind}
            onChange={(event) => { setFromKind(event.target.value) }}
          />
          <Button variant="secondary" size="sm" disabled={busy || !from} onClick={() => { void injectFromContainer() }}>{text.resourceInjectFrom}</Button>
        </div>

        <div className="resource-footer">
          <Button variant="ghost" size="sm" onClick={onClose}>{text.resourceClose}</Button>
        </div>
      </div>
      {editOpen && picked !== null && (
        <EditYamlDialog
          containerId={id}
          path={picked}
          text={text}
          onClose={() => { setEditOpen(false); void reload(plugin) }}
        />
      )}
    </Dialog>
  )
}
