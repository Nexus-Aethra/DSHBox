import { useEffect, useState } from 'react'
import type { ContainerExtensions, DshContainer, ExtensionBundle, RepositoryExtension, WorkspaceExtension } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Dialog } from '../../ui/Dialog'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'
import { Tabs } from '../../ui/Tabs'
import { Toolbar } from '../../ui/Toolbar'
import { Stack } from '../../ui/Stack'

export type ContainerDetailsText = {
  back: string; activeProfile: string; profiles: string; addProfile: string; profilePlaceholder: string
  plugins: string; pluginRepo: string; skills: string; logs: string
  hostLog: string; rebuildLog: string; webviewLog: string; logRefresh: string
  workspace: string; noPlugins: string; noSkills: string; containerSkill: string
  diagnostics: string; version: string; addExtension: string; upgrade: string; remove: string
  scanWorkspace: string; importWorkspace: string; extensionSource: string; browseArchive: string
  adding: string; openInBrowser: string; singleAdd: string; bundleAdd: string
  conflictOverwrite: string; conflictKeep: string
  profilesTab: string; pluginsTab: string; skillsTab: string; logsTab: string
  confirmTitle: string; dialogCancel: string; dialogConfirm: string
  deletePluginConfirm: (name: string) => string
  scanWorkspaceEmpty: string; importWorkspaceHelp: string
}
type Tab = 'profiles' | 'plugins' | 'skills' | 'logs'
type Props = { container: DshContainer; details: ContainerExtensions | null; repository: RepositoryExtension[]; bundles: ExtensionBundle[]; workspaceExtensions: WorkspaceExtension[]; text: ContainerDetailsText; onBack: () => void; onAddProfile: (profile: string) => Promise<void>; onSelectProfile: (profile: string) => Promise<void>; onAddExtension: (profile: string | null, repositoryId: string) => Promise<void>; onAddBundle: (profile: string, bundleId: string, conflict: string) => Promise<void>; onDeletePlugin: (profile: string, name: string) => Promise<void>; onScanWorkspace: () => Promise<void>; onImportWorkspace: (relativePath: string) => Promise<void>; onReadLog: (id: string, log: 'host' | 'rebuild' | 'webview') => Promise<string>; onOpenInBrowser: (id: string) => Promise<void> }

export function ContainerDetails({ container, details, repository, bundles, workspaceExtensions, text, onBack, onAddProfile, onSelectProfile, onAddExtension, onAddBundle, onDeletePlugin, onScanWorkspace, onImportWorkspace, onReadLog, onOpenInBrowser }: Props) {
  const [profile, setProfile] = useState(container.profile)
  const [newProfile, setNewProfile] = useState('')
  const [source, setSource] = useState('')
  const [saving, setSaving] = useState(false)
  const [tab, setTab] = useState<Tab>('profiles')
  const [logKind, setLogKind] = useState<'host' | 'rebuild' | 'webview'>('host')
  const [log, setLog] = useState('')
  const [addMode, setAddMode] = useState<'single' | 'bundle'>('single')
  const [bundleId, setBundleId] = useState('')
  const [conflict, setConflict] = useState('keep')
  const [pendingDelete, setPendingDelete] = useState<string | null>(null)
  useEffect(() => { setProfile(container.profile) }, [container.id, container.profile])
  const selected = details?.profiles.find((item) => item.name === profile) ?? details?.profiles[0]
  const repositoryPlugins = repository.filter((entry) => entry.kind === 'plugin' && !entry.diagnostic)
  const repositorySkills = repository.filter((entry) => entry.kind === 'skill' && !entry.diagnostic)
  const workspaceCandidates = workspaceExtensions.filter((entry) => entry.kind === (tab === 'plugins' ? 'plugin' : 'skill'))
  async function selectProfile(value: string, persist: boolean): Promise<void> { if (!persist) { setProfile(value); return }; setSaving(true); try { await onSelectProfile(value); setProfile(value) } finally { setSaving(false) } }
  async function addProfile(): Promise<void> { const value = newProfile.trim(); if (!value) return; setSaving(true); try { await onAddProfile(value); setProfile(value); setNewProfile('') } finally { setSaving(false) } }
  async function addExtension(kind: 'plugin' | 'skill'): Promise<void> { const value = source.trim(); if (!value || (kind === 'plugin' && !selected)) return; setSaving(true); try { await onAddExtension(kind === 'plugin' ? selected!.name : null, value); setSource('') } finally { setSaving(false) } }
  async function addBundle(): Promise<void> { if (!bundleId || !profile) return; setSaving(true); try { await onAddBundle(profile, bundleId, conflict); setBundleId('') } finally { setSaving(false) } }
  async function upgrade(repositoryId: string): Promise<void> { if (!selected) return; setSaving(true); try { await onAddExtension(selected.name, repositoryId) } finally { setSaving(false) } }
  async function deletePlugin(name: string): Promise<void> { if (!selected) return; setSaving(true); try { await onDeletePlugin(selected.name, name) } finally { setSaving(false) } }
  async function loadLog(kind = logKind): Promise<void> { setLogKind(kind); setLog(await onReadLog(container.id, kind)) }
  async function confirmDelete(): Promise<void> { if (pendingDelete === null || !selected) return; const name = pendingDelete; setPendingDelete(null); await deletePlugin(name) }

  const tabItems: { id: Tab; label: string }[] = [
    { id: 'profiles', label: text.profilesTab },
    { id: 'plugins', label: text.pluginsTab },
    { id: 'skills', label: text.skillsTab },
    { id: 'logs', label: text.logsTab },
  ]

  return (
    <section className="workspace container-detail-view">
      <button type="button" className="back-button" onClick={onBack}>← {text.back}</button>
      <p className="eyebrow">CONTAINER</p>
      <h1>{container.name}</h1>
      <section className="container-detail-summary">
        <span>{container.id}</span>
        <span>{text.version}: {container.version}</span>
        <span>{text.workspace}: {container.directory}/workspace</span>
      </section>

      <Tabs items={tabItems} value={tab} onChange={(next) => { setTab(next); if (next === 'logs') void loadLog() }} ariaLabel={text.profiles}>

        {tab === 'profiles' && (
          <Card>
            <Stack gap={4}>
              <div className="extensions-heading-row">
                <h2 className="extensions-heading-title">{text.profiles}</h2>
                <Field label={text.activeProfile}>
                  {(id) => <Select id={id} value={container.profile} disabled={saving || !selected} onChange={(event) => { void selectProfile(event.target.value, true) }} options={(details?.profiles ?? []).map((item) => ({ value: item.name, label: item.name }))} />}
                </Field>
              </div>
              <Toolbar>
                <Input value={newProfile} placeholder={text.profilePlaceholder} onChange={(event) => { setNewProfile(event.target.value) }} />
                <Button variant="primary" disabled={saving || !newProfile.trim()} onClick={() => { void addProfile() }}>{text.addProfile}</Button>
              </Toolbar>
            </Stack>
          </Card>
        )}

        {(tab === 'plugins' || tab === 'skills') && (
          <>
            <Card>
              <div className="extensions-heading-row">
                <h2 className="extensions-heading-title">{text[tab]}</h2>
                {tab === 'plugins' && (
                  <Field label={text.profiles}>
                    {(id) => <Select id={id} value={selected?.name ?? ''} disabled={saving || !selected} onChange={(event) => { void selectProfile(event.target.value, false) }} options={(details?.profiles ?? []).map((item) => ({ value: item.name, label: item.name }))} />}
                  </Field>
                )}
              </div>
              <Toolbar>
                <Select value={addMode} disabled={saving} onChange={(event) => { setAddMode(event.target.value as 'single' | 'bundle') }} options={[{ value: 'single', label: text.singleAdd }, { value: 'bundle', label: text.bundleAdd }]} />
                {addMode === 'single' ? (
                  <>
                    <Select value={source} disabled={saving || (tab === 'plugins' && !selected)} onChange={(event) => { setSource(event.target.value) }} options={[{ value: '', label: text.pluginRepo }, ...(tab === 'plugins' ? repositoryPlugins : repositorySkills).map((entry) => ({ value: entry.id, label: `${entry.name}${entry.version ? ` · ${entry.version}` : ''}` }))]} />
                    <Button variant="primary" disabled={saving || !source || (tab === 'plugins' && !selected)} loading={saving} onClick={() => { void addExtension(tab === 'plugins' ? 'plugin' : 'skill') }}>{saving ? text.adding : text.addExtension}</Button>
                  </>
                ) : (
                  <>
                    <Select value={bundleId} disabled={saving || bundles.length === 0} onChange={(event) => { setBundleId(event.target.value) }} options={[{ value: '', label: text.bundleAdd }, ...bundles.map((bundle) => ({ value: bundle.id, label: `${bundle.name} · ${bundle.entries.length} extension${bundle.entries.length === 1 ? '' : 's'}` }))]} />
                    <Select value={conflict} disabled={saving} onChange={(event) => { setConflict(event.target.value) }} options={[{ value: 'keep', label: text.conflictKeep }, { value: 'overwrite', label: text.conflictOverwrite }]} />
                    <Button variant="primary" disabled={saving || !bundleId || (tab === 'plugins' && !selected)} loading={saving} onClick={() => { void addBundle() }}>{saving ? text.adding : text.addExtension}</Button>
                  </>
                )}
              </Toolbar>
            </Card>

            <Card padding="sm">
              <Toolbar justify="between">
                <span className="ui-field-label">{text.scanWorkspace}</span>
                <Button variant="secondary" size="sm" disabled={saving} onClick={() => { void onScanWorkspace() }}>{text.scanWorkspace}</Button>
              </Toolbar>
              {workspaceCandidates.length > 0 ? (
                <div className="ui-stack gap-2">
                  {workspaceCandidates.map((entry) => (
                    <div key={entry.relativePath} className="workspace-extension-row">
                      <div>
                        <strong>{entry.name}</strong>
                        <p>{entry.relativePath}</p>
                      </div>
                      <Button variant="secondary" size="sm" disabled={saving || Boolean(entry.diagnostic)} onClick={() => { void onImportWorkspace(entry.relativePath) }}>{text.importWorkspace}</Button>
                    </div>
                  ))}
                </div>
              ) : <p className="ui-field-help">{text.importWorkspaceHelp}</p>}
            </Card>

            {tab === 'plugins' ? (
              <div className="extension-list">
                {selected?.plugins.length ? selected.plugins.map((plugin) => {
                  const candidate = repositoryPlugins.find((entry) => entry.name === plugin.name && entry.version !== plugin.version)
                  return (
                    <article key={plugin.name} className="extension-row">
                      <div><strong>{plugin.name}</strong><p>{plugin.description ?? plugin.diagnostic ?? ''}</p></div>
                      <div className="extension-row-actions">
                        <code>{plugin.version ?? '—'}</code>
                        {candidate && <Button variant="secondary" size="sm" disabled={saving} onClick={() => { void upgrade(candidate.id) }}>{text.upgrade}</Button>}
                        <Button variant="danger" size="sm" disabled={saving} onClick={() => { setPendingDelete(plugin.name) }}>{text.remove}</Button>
                      </div>
                    </article>
                  )
                }) : <p className="empty-extension">{text.noPlugins}</p>}
              </div>
            ) : (
              <div className="extension-list">
                {details?.skills.length ? details.skills.map((skill) => (
                  <article key={skill.path} className="extension-row">
                    <div><strong>{skill.name}</strong><p>{skill.description ?? skill.diagnostic ?? ''}</p></div>
                    <div>
                      <Badge variant="primary">{text.containerSkill}</Badge>
                      <p className="path">{skill.path}</p>
                    </div>
                  </article>
                )) : <p className="empty-extension">{text.noSkills}</p>}
              </div>
            )}
          </>
        )}

        {tab === 'logs' && (
          <Card>
            <Stack gap={4}>
              <div className="extensions-heading-row">
                <h2 className="extensions-heading-title">{text.logs}</h2>
                <Toolbar>
                  <Button variant={logKind === 'host' ? 'primary' : 'secondary'} size="sm" onClick={() => { void loadLog('host') }}>{text.hostLog}</Button>
                  <Button variant={logKind === 'webview' ? 'primary' : 'secondary'} size="sm" onClick={() => { void loadLog('webview') }}>{text.webviewLog}</Button>
                  <Button variant={logKind === 'rebuild' ? 'primary' : 'secondary'} size="sm" onClick={() => { void loadLog('rebuild') }}>{text.rebuildLog}</Button>
                  <Button variant="secondary" size="sm" onClick={() => { void loadLog() }}>{text.logRefresh}</Button>
                  <Button variant="secondary" size="sm" onClick={() => { void onOpenInBrowser(container.id) }}>↗ {text.openInBrowser}</Button>
                </Toolbar>
              </div>
              <pre className="container-log">{log || '…'}</pre>
            </Stack>
          </Card>
        )}

      </Tabs>

      <Dialog
        open={pendingDelete !== null}
        title={text.confirmTitle}
        description={pendingDelete === null ? '' : text.deletePluginConfirm(pendingDelete)}
        onClose={() => { setPendingDelete(null) }}
      >
        <div className="ui-dialog-actions">
          <Button variant="secondary" size="sm" onClick={() => { setPendingDelete(null) }}>{text.dialogCancel}</Button>
          <Button variant="danger" size="sm" onClick={() => { void confirmDelete() }}>{text.dialogConfirm}</Button>
        </div>
      </Dialog>

      {details?.diagnostics.length ? <section className="extension-diagnostics"><strong>{text.diagnostics}</strong>{details.diagnostics.map((item) => <p key={item}>{item}</p>)}</section> : null}
    </section>
  )
}
