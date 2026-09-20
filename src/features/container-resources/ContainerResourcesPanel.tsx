import { useEffect, useMemo, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { ContainerPathListing, ContainerResources, DiscoveredResource, DshContainer, StoredResource } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'

export type ResourceText = {
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
  resourcePluginsAvailable: string
  resourcePluginAll: string
  resourceNoPluginMatch: string
  resourceBrowse: string
  resourceBrowseUp: string
  resourceBrowseRoot: string
  resourceBrowsePick: string
  resourceBrowseSelected: (path: string) => string
  resourceBrowseExtract: string
  resourceBrowseInject: string
  resourceBrowseInjectPick: string
  resourceBrowseEmpty: string
  resourceBrowseSymlink: string
  resourceBrowseChildren: (count: number) => string
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
  const [pluginQuery, setPluginQuery] = useState('')
  const [installedPlugins, setInstalledPlugins] = useState<string[]>([])
  const [browseOpen, setBrowseOpen] = useState(false)
  const [listing, setListing] = useState<ContainerPathListing | null>(null)
  const [picked, setPicked] = useState<string | null>(null)
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
    boxApi.getContainerDetails(id).then((details) => {
      const names = new Set<string>()
      for (const profile of details?.profiles ?? []) {
        for (const entry of profile.plugins ?? []) if (entry.name) names.add(entry.name)
      }
      setInstalledPlugins([...names].sort())
    }).catch(() => { setInstalledPlugins([]) })
    // Reloading on container change is the whole point; `plugin` is applied by
    // the Detect button so a half-typed package name never hits the daemon.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  // Subsequence match, so `dsh ws` finds `@nexus-aethra/dshell-workspace`; a
  // query that starts the name ranks first.
  const pluginMatches = useMemo(() => {
    const query = pluginQuery.trim().toLowerCase()
    if (!query) return installedPlugins.slice(0, 8).map((name) => ({ name, score: 0 }))
    return installedPlugins
      .map((name) => {
        const haystack = name.toLowerCase()
        const at = haystack.indexOf(query)
        if (at >= 0) return { name, score: at === 0 ? 0 : 1 }
        let cursor = 0
        for (const character of query) {
          cursor = haystack.indexOf(character, cursor)
          if (cursor < 0) return null
          cursor += 1
        }
        return { name, score: 2 }
      })
      .filter((entry): entry is { name: string; score: number } => entry !== null)
      .sort((left, right) => left.score - right.score || left.name.localeCompare(right.name))
      .slice(0, 8)
  }, [installedPlugins, pluginQuery])

  async function browse(path: string): Promise<void> {
    try {
      setListing(await boxApi.browseContainerPaths(id, path))
      setPicked(null)
      setBrowseOpen(true)
    } catch (browseError) {
      setError(String(browseError))
    }
  }

  async function extractPickedPath(): Promise<void> {
    if (picked === null) return
    const name = picked.split('/').filter(Boolean).pop() ?? picked
    // The kind is what the user picked, so the record reads `path-<last segment>`
    // instead of repeating the segment as both kind and name.
    await run(() => boxApi.enqueueResourceExtract({ id, kind: 'path', dest: picked, name }))
  }

  async function injectIntoPickedPath(): Promise<void> {
    if (picked === null || !injectPick) return
    await run(() => boxApi.enqueueResourceInject({ id, resource: injectPick, dest: picked, conflict, restart }))
  }

  const others = useMemo(() => containers.filter((container) => container.id !== id), [containers, id])

  async function run(work: () => Promise<{ id: string }>): Promise<void> {
    setBusy(true)
    setError(null)
    try {
      const task = await work()
      setQueued(task.id ?? '')
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

        <div className="resource-detect">
          <div className="resource-combo">
            <Input
              size="sm"
              value={pluginQuery}
              placeholder={text.resourcePluginPlaceholder}
              aria-label={text.resourcePlugin}
              onChange={(event) => { setPluginQuery(event.target.value) }}
            />
            <ul className="resource-combo-list" aria-label={text.resourcePluginsAvailable}>
              <li>
                <button
                  type="button"
                  className={plugin === '' ? 'active' : ''}
                  onClick={() => { setPlugin(''); setPluginQuery(''); void reload('') }}
                >{text.resourcePluginAll}</button>
              </li>
              {pluginMatches.map((entry) => (
                <li key={entry.name}>
                  <button
                    type="button"
                    className={plugin === entry.name ? 'active' : ''}
                    onClick={() => { setPlugin(entry.name); setPluginQuery(entry.name); void reload(entry.name) }}
                  >{entry.name}</button>
                </li>
              ))}
              {pluginMatches.length === 0 && <li className="resource-combo-empty">{text.resourceNoPluginMatch}</li>}
            </ul>
          </div>
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => { void reload(plugin) }}>{text.resourceDetect}</Button>
          <Button variant="ghost" size="sm" onClick={() => { void browse('profile') }}>{text.resourceBrowse}</Button>
        </div>

        {browseOpen && (
          <div className="resource-browser">
            <div className="resource-browser-head">
              <Button variant="ghost" size="sm" disabled={listing === null || listing.path === '.'} onClick={() => { if (listing) void browse(listing.parent) }}>{text.resourceBrowseUp}</Button>
              <code>{listing?.path ?? text.resourceBrowseRoot}</code>
            </div>
            <ul className="resource-browser-list">
              {(listing?.entries ?? []).map((entry) => (
                <li key={entry.path}>
                  {entry.directory
                    ? <button type="button" className="resource-browser-open" onClick={() => { void browse(entry.path) }}>▸ {entry.name}</button>
                    : <span className="resource-browser-name">{entry.name} <span className="resource-browser-meta">{human(entry.bytes)}</span></span>}
                  {/* A directory is entered by its name and picked by this
                      button: extracting a whole session directory is the
                      common case, so directories must be selectable too. */}
                  <button
                    type="button"
                    className={picked === entry.path ? 'active' : ''}
                    onClick={() => { setPicked(entry.path) }}
                  >{text.resourceBrowsePick}</button>
                  {entry.symlink && <span className="resource-browser-meta">{text.resourceBrowseSymlink}</span>}
                  {entry.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
                  {entry.directory && <span className="resource-browser-meta">{text.resourceBrowseChildren(entry.children)}</span>}
                </li>
              ))}
              {(listing?.entries.length ?? 0) === 0 && <li className="resource-note">{text.resourceBrowseEmpty}</li>}
            </ul>
            <div className="resource-browser-pick">
              <span className="resource-note">{text.resourceBrowsePick}</span>
              {picked !== null && <code>{text.resourceBrowseSelected(picked)}</code>}
              <Button variant="secondary" size="sm" disabled={busy || picked === null} onClick={() => { void extractPickedPath() }}>{text.resourceBrowseExtract}</Button>
              <Select
                value={injectPick}
                placeholder={text.resourceBrowseInjectPick}
                aria-label={text.resourceBrowseInject}
                options={(data?.stored ?? []).map((resource) => ({ value: resource.id, label: resource.id }))}
                onChange={(event) => { setInjectPick(event.target.value) }}
              />
              <Button variant="secondary" size="sm" disabled={busy || picked === null || !injectPick} onClick={() => { void injectIntoPickedPath() }}>{text.resourceBrowseInject}</Button>
            </div>
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
    </Dialog>
  )
}
