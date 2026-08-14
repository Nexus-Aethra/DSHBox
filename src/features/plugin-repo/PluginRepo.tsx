import { useState } from 'react'
import type { RepositoryExtension } from '../../shared/types/domain'

type Text = { pluginRepo: string; pluginRepoNote: string; noRepositoryPlugins: string; exportPlugin: string; remove: string; extensionSource: string; browseArchive: string; addExtension: string }
type Props = { entries: RepositoryExtension[]; text: Text; onImport: (source: string) => Promise<void>; onChooseArchive: () => Promise<string | null>; onExport: (entry: RepositoryExtension) => Promise<void>; onDelete: (id: string) => Promise<void> }

export function PluginRepo({ entries, text, onImport, onChooseArchive, onExport, onDelete }: Props) {
  const [source, setSource] = useState('')
  async function browse(): Promise<void> { const path = await onChooseArchive(); if (path) setSource(path) }
  async function add(): Promise<void> { if (!source.trim()) return; await onImport(source.trim()); setSource('') }
  return <section className="workspace"><p className="eyebrow">PLUGINS</p><h1>{text.pluginRepo}</h1><p className="workspace-note">{text.pluginRepoNote}</p><section className="plugin-repo-import"><input value={source} placeholder={text.extensionSource} onChange={(event) => { setSource(event.target.value) }} /><button type="button" className="secondary" onClick={() => { void browse() }}>{text.browseArchive}</button><button type="button" className="primary" disabled={!source.trim()} onClick={() => { void add() }}>{text.addExtension}</button></section><div className="extension-list plugin-repo-list">{entries.length ? entries.map((entry) => <article key={entry.id} className="extension-row"><div><strong>{entry.name}</strong><p>{entry.description ?? entry.diagnostic ?? ''}</p></div><div className="plugin-repo-actions"><span className="extension-source">{entry.kind}</span><code>{entry.version ?? '—'}</code><button type="button" className="secondary" onClick={() => { void onExport(entry) }}>{text.exportPlugin}</button><button type="button" className="secondary danger-button" onClick={() => { if (window.confirm(`Remove ${entry.name}?`)) void onDelete(entry.id) }}>{text.remove}</button></div></article>) : <p className="empty-extension">{text.noRepositoryPlugins}</p>}</div></section>
}
