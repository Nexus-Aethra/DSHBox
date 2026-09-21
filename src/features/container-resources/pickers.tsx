import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { ContainerPathListing } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Input } from '../../ui/Input'

export type PickerText = {
  resourcePluginsAvailable: string
  resourcePluginAll: string
  resourcePluginPlaceholder: string
  resourcePlugin: string
  resourceNoPluginMatch: string
  resourceBrowse: string
  resourceBrowseUp: string
  resourceBrowseRoot: string
  resourceBrowsePick: string
  resourceBrowseEmpty: string
  resourceBrowseSymlink: string
  resourceBrowseChildren: (count: number) => string
  resourceSecret: string
}

/**
 * Every package installed in the container's profile. The daemon reads the
 * profile's `node_modules`, so a package a bundle pulled in transitively is
 * offered too — the declared plugin list alone would hide it.
 */
export function useInstalledPlugins(id: string): string[] {
  const [plugins, setPlugins] = useState<string[]>([])
  useEffect(() => {
    let cancelled = false
    boxApi.listContainerResources(id)
      .then((data) => { if (!cancelled) setPlugins(data.plugins ?? []) })
      .catch(() => { if (!cancelled) setPlugins([]) })
    return () => { cancelled = true }
  }, [id])
  return plugins
}

/**
 * Subsequence match, so `dsh ws` finds `@nexus-aethra/dshell-workspace`. No
 * result cap: the chip list scrolls, and hiding package 9 would hide the one
 * the user is looking for.
 */
export function matchPlugins(installed: string[], query: string): string[] {
  const needle = query.trim().toLowerCase()
  if (!needle) return installed
  return installed
    .map((name) => {
      const haystack = name.toLowerCase()
      const at = haystack.indexOf(needle)
      if (at >= 0) return { name, score: at === 0 ? 0 : 1 }
      let cursor = 0
      for (const character of needle) {
        cursor = haystack.indexOf(character, cursor)
        if (cursor < 0) return null
        cursor += 1
      }
      return { name, score: 2 }
    })
    .filter((entry): entry is { name: string; score: number } => entry !== null)
    .sort((left, right) => left.score - right.score || left.name.localeCompare(right.name))
    .map((entry) => entry.name)
}

/** Installed plugins as chips, filtered as you type. */
export function PluginChips({ plugins, query, onQuery, selected, onSelect, text }: {
  plugins: string[]
  query: string
  onQuery: (next: string) => void
  selected: string
  onSelect: (name: string) => void
  text: PickerText
}) {
  const matches = matchPlugins(plugins, query)
  return (
    <div className="resource-detect">
      <div className="resource-combo">
        <Input
          size="sm"
          value={query}
          placeholder={text.resourcePluginPlaceholder}
          aria-label={text.resourcePlugin}
          onChange={(event) => { onQuery(event.target.value) }}
        />
        <ul className="resource-combo-list" aria-label={text.resourcePluginsAvailable}>
          <li>
            <button type="button" className={selected === '' ? 'active' : ''} onClick={() => { onSelect(''); onQuery('') }}>
              {text.resourcePluginAll}
            </button>
          </li>
          {matches.map((name) => (
            <li key={name}>
              <button type="button" className={selected === name ? 'active' : ''} onClick={() => { onSelect(name); onQuery(name) }}>
                {name}
              </button>
            </li>
          ))}
          {matches.length === 0 && <li className="resource-combo-empty">{text.resourceNoPluginMatch}</li>}
        </ul>
      </div>
    </div>
  )
}

/**
 * The container's storage area as a tree. Directories are entered by name and
 * picked by button (the common case is a whole directory, e.g. one session),
 * files show their size, and anything that looks like a secret is flagged.
 */
export function ContainerTree({ id, start = 'profile', text, onPick }: {
  id: string
  start?: string
  text: PickerText
  onPick: (path: string) => void
}) {
  const [listing, setListing] = useState<ContainerPathListing | null>(null)
  const [picked, setPicked] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function browse(path: string): Promise<void> {
    try {
      setListing(await boxApi.browseContainerPaths(id, path))
      setError(null)
    } catch (browseError) {
      setError(String(browseError))
    }
  }

  useEffect(() => { void browse(start); setPicked(null) }, [id, start])

  function human(bytes: number): string {
    const units = ['B', 'KB', 'MB', 'GB']
    let value = bytes
    let unit = 0
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1 }
    return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`
  }

  return (
    <div className="resource-browser">
      <div className="resource-browser-head">
        <Button variant="ghost" size="sm" disabled={listing === null || listing.path === '.'} onClick={() => { if (listing) void browse(listing.parent) }}>{text.resourceBrowseUp}</Button>
        <code>{listing?.path ?? text.resourceBrowseRoot}</code>
      </div>
      {error !== null && <p className="resource-error">{error}</p>}
      <ul className="resource-browser-list">
        {(listing?.entries ?? []).map((entry) => (
          <li key={entry.path}>
            {entry.directory
              ? <button type="button" className="resource-browser-open" onClick={() => { void browse(entry.path) }}>▸ {entry.name}</button>
              : <span className="resource-browser-name">{entry.name} <span className="resource-browser-meta">{human(entry.bytes)}</span></span>}
            <button
              type="button"
              className={picked === entry.path ? 'active' : ''}
              onClick={() => { setPicked(entry.path); onPick(entry.path) }}
            >{text.resourceBrowsePick}</button>
            {entry.symlink && <span className="resource-browser-meta">{text.resourceBrowseSymlink}</span>}
            {entry.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
            {entry.directory && <span className="resource-browser-meta">{text.resourceBrowseChildren(entry.children)}</span>}
          </li>
        ))}
        {(listing?.entries.length ?? 0) === 0 && <li className="resource-note">{text.resourceBrowseEmpty}</li>}
      </ul>
    </div>
  )
}
