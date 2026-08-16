import { useEffect, useState } from 'react'
import type { DshVersion, ExtensionBundle, HarnessUpgradeReport, PreviewScriptResult, RepositoryExtension, TemplateInfo } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Checkbox } from '../../ui/Checkbox'
import { Dialog } from '../../ui/Dialog'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'
import { Tabs } from '../../ui/Tabs'
import { Toolbar } from '../../ui/Toolbar'

type Text = {
  pluginRepo: string; pluginRepoNote: string; noRepositoryPlugins: string; exportPlugin: string
  remove: string; extensionSource: string; browseArchive: string; addExtension: string
  pluginsTab: string; bundles: string; createBundle: string; bundleName: string
  selectEntries: string; quickExport: string; fullExport: string; noBundles: string
  githubOnly: string; importBundle: string; conflictOverwrite: string; conflictKeep: string
  bundleRefNote: string; bundleRefDelete: string
  resources: string; harnessTab: string; templateTab: string; bundleTab: string; addResource: string
  versionTitle: string; versionNote: string; noVersion: string; install: string; installed: string
  uninstall: string; loadVersions: string; installing: string
  buildScript: string; scriptPath: string; chooseScript: string; previewScript: string
  buildTemplate: string; noScriptPreview: string; operations: string; scriptPreviewTitle: string
  importTemplate: string; exportTemplate: string; removeTemplate: string; browseExport: string
  deleteTemplateConfirm: (name: string) => string
  checkUpdates: string; upgradeRun: string; templateReady: string
  templatePending: string; upgradeDone: string; upToDate: string; upgradeFailed: string
  noTemplate: string; templateName: string; templateHarness: string
  pickEntries: string; bundleExtensionCount: (n: number) => string
  confirmTitle: string; dialogCancel: string; dialogConfirm: string
  deleteBundleConfirm: (name: string) => string; deletePluginConfirm: (name: string) => string
  deletePluginWithRefs: (name: string, refs: string) => string
  usedByContainers: (n: number) => string
}

type Props = {
  plugins: RepositoryExtension[]
  bundles: ExtensionBundle[]
  references: Record<string, number>
  dshVersions: DshVersion[]
  installedDshVersions: string[]
  installingVersion: string | null
  loadingVersions: boolean
  upgradingResources: boolean
  upgradeReport: HarnessUpgradeReport[] | null
  activeTaskFor: (resourceKey: string) => { stage: string } | undefined
  text: Text
  onImportPlugin: (source: string) => Promise<void>
  onChooseArchive: () => Promise<string | null>
  onExportPlugin: (entry: RepositoryExtension) => Promise<void>
  onDeletePlugin: (id: string) => Promise<void>
  onLoadBundles: () => Promise<void>
  onCreateBundle: (name: string, ids: string[]) => Promise<void>
  onDeleteBundle: (id: string) => Promise<void>
  onExportBundle: (bundle: ExtensionBundle, mode: string) => Promise<void>
  onImportBundle: (archive: string, conflict: string) => Promise<void>
  onInstallDshVersion: (version: string) => Promise<void>
  onUninstallDshVersion: (version: string) => Promise<void>
  onRefreshDshCatalog: () => Promise<void>
  onUpgradeResources: () => Promise<void>
  onReloadPlugins: () => Promise<void>
  onLoadTemplates: () => Promise<void>
  onChooseScript: () => Promise<string | null>
  onBuildTemplate: (scriptPath: string) => Promise<void>
  onImportTemplate: (archive: string) => Promise<void>
  onChooseExportDestination: (defaultName: string) => Promise<string | null>
  onExportTemplate: (name: string, destination: string) => Promise<void>
  onRemoveTemplate: (name: string) => Promise<void>
  scriptPreview: PreviewScriptResult | null
  templates: TemplateInfo[]
}

const isGithub = (source: string | null) => !!source && source.startsWith('https://github.com/')

type TabId = 'harness' | 'plugins' | 'bundles' | 'template'

export function ResourcesPage({
  plugins, bundles, references, dshVersions, installedDshVersions, installingVersion, loadingVersions,
  upgradingResources, upgradeReport,
  activeTaskFor, text,
  onImportPlugin, onChooseArchive, onExportPlugin, onDeletePlugin,
  onLoadBundles, onCreateBundle, onDeleteBundle, onExportBundle, onImportBundle,
  onInstallDshVersion, onUninstallDshVersion, onRefreshDshCatalog, onUpgradeResources,
    onReloadPlugins, onLoadTemplates,
  onChooseScript, onBuildTemplate,
  onImportTemplate, onChooseExportDestination, onExportTemplate, onRemoveTemplate,
  scriptPreview,
  templates,
}: Props) {
  const [tab, setTab] = useState<TabId>('harness')
  const [source, setSource] = useState('')
  const [bundleName, setBundleName] = useState('')
  const [selected, setSelected] = useState<string[]>([])
  const [bundleArchive, setBundleArchive] = useState('')
  const [conflict, setConflict] = useState('keep')
  const [scriptPath, setScriptPath] = useState('')
  const [pendingDelete, setPendingDelete] = useState<{ kind: 'plugin' | 'bundle'; id: string; name: string; refs: string[] } | null>(null)
  const [pendingTemplateDelete, setPendingTemplateDelete] = useState<{ name: string } | null>(null)

  // Tab-scoped loading: each tab refreshes its own data the moment it is
  // shown, so a template pulled from the CLI shows up as soon as the user
  // opens the Template tab without any manual refresh.
  useEffect(() => {
    if (tab === 'plugins') void onReloadPlugins()
    if (tab === 'bundles') void onLoadBundles()
    if (tab === 'template') void onLoadTemplates()
    if (tab === 'harness') void onRefreshDshCatalog()
  }, [tab])

  async function browse(): Promise<void> { const path = await onChooseArchive(); if (path) setSource(path) }
  async function addPlugin(): Promise<void> { if (!source.trim()) return; await onImportPlugin(source.trim()); setSource('') }
  async function createBundle(): Promise<void> { const name = bundleName.trim(); if (!name || selected.length === 0) return; await onCreateBundle(name, selected); setBundleName(''); setSelected([]) }
  async function browseBundleArchive(): Promise<void> { const path = await onChooseArchive(); if (path) setBundleArchive(path) }
  async function importBundle(): Promise<void> { if (!bundleArchive.trim()) return; await onImportBundle(bundleArchive.trim(), conflict); setBundleArchive('') }
  async function browseScript(): Promise<void> { const path = await onChooseScript(); if (path) setScriptPath(path) }
  async function doBuild(): Promise<void> { if (!scriptPath.trim()) return; await onBuildTemplate(scriptPath.trim()); setScriptPath('') }
  async function importTemplateArchive(): Promise<void> {
    const archive = await onChooseArchive();
    if (archive) await onImportTemplate(archive)
  }
  async function exportTemplateArchive(templateName: string): Promise<void> {
    const destination = await onChooseExportDestination(`${templateName}.dsh.tar.gz`);
    if (destination) await onExportTemplate(templateName, destination)
  }
  function requestDeleteTemplate(templateName: string): void {
    setPendingTemplateDelete({ name: templateName })
  }
  async function confirmTemplateDelete(): Promise<void> {
    if (pendingTemplateDelete === null) return
    const { name } = pendingTemplateDelete
    setPendingTemplateDelete(null)
    await onRemoveTemplate(name)
  }

  function requestDeletePlugin(entry: RepositoryExtension): void {
    const refs = bundles.filter((bundle) => bundle.entries.some((item) => item.repositoryId === entry.id)).map((bundle) => bundle.name)
    setPendingDelete({ kind: 'plugin', id: entry.id, name: entry.name, refs })
  }
  function requestDeleteBundle(bundle: ExtensionBundle): void {
    setPendingDelete({ kind: 'bundle', id: bundle.id, name: bundle.name, refs: [] })
  }
  async function confirmDelete(): Promise<void> {
    if (pendingDelete === null) return
    const { kind, id } = pendingDelete
    setPendingDelete(null)
    if (kind === 'plugin') await onDeletePlugin(id)
    else await onDeleteBundle(id)
  }

  function toggleSelected(id: string, checked: boolean): void {
    setSelected((current) => checked ? [...new Set([...current, id])] : current.filter((value) => value !== id))
  }

  const formatSize = (bytes: number) => bytes > 1048576 ? `${(bytes / 1048576).toFixed(1)} MB` : bytes > 1024 ? `${Math.round(bytes / 1024)} KB` : `${bytes} B`
  const totalSize = (bundle: ExtensionBundle) => bundle.entries.reduce((sum, entry) => sum + entry.size, 0)

  const tabItems: { id: TabId; label: string }[] = [
    { id: 'harness', label: text.harnessTab },
    { id: 'plugins', label: text.pluginsTab },
    { id: 'bundles', label: text.bundleTab },
    { id: 'template', label: text.templateTab },
  ]

  return (
    <section className="workspace">
      <p className="eyebrow">RESOURCES</p>
      <h1>{text.resources}</h1>
      <Tabs items={tabItems} value={tab} onChange={setTab} ariaLabel={text.resources}>

        {tab === 'harness' && (
          <>
            <p className="workspace-note">{text.versionNote}</p>
            <Toolbar>
              <Button variant="secondary" size="sm" disabled={loadingVersions || upgradingResources} onClick={() => { void onRefreshDshCatalog() }}>{loadingVersions ? '…' : text.loadVersions}</Button>
              <Button variant="secondary" size="sm" disabled={loadingVersions || upgradingResources} onClick={() => { void onUpgradeResources() }}>{upgradingResources ? text.upgradeRun : text.checkUpdates}</Button>
            </Toolbar>
            {upgradeReport !== null && (upgradeReport.length > 0
              ? <p className="upgrade-note">{text.upgradeDone
                  .replace('{created}', String(upgradeReport.filter((report) => report.templateCreated).length))}</p>
              : <p className="upgrade-note">{text.upToDate}</p>)}
            {dshVersions.length > 0 ? (
              <div className="version-list">
                {dshVersions.map((version) => {
                  const task = activeTaskFor(`runtime:${version.name}`)
                  const busy = task !== undefined
                  const reportFor = upgradeReport?.find((report) => report.version === version.name)
                  return (
                    <section key={version.name} className="version-row">
                      <div className="harness-info">
                        <code>{version.name}</code>
                        {version.installed && reportFor && (
                          <p className="harness-ref"><span className="label">{reportFor.templateCreated ? text.templateReady : text.templatePending}</span> <code>{reportFor.templatePath}</code></p>
                        )}
                      </div>
                      <div className="version-actions">
                        {version.installed ? (
                          <>
                            <Badge variant="success">{text.installed}</Badge>
                            <Button variant="secondary" size="sm" disabled={busy} onClick={() => { void onUninstallDshVersion(version.name) }}>{text.uninstall}</Button>
                          </>
                        ) : (
                          <Button variant="primary" size="sm" disabled={busy || installingVersion !== null} onClick={() => { void onInstallDshVersion(version.name) }}>
                            {busy || installingVersion === version.name ? text.installing : text.install}
                          </Button>
                        )}
                      </div>
                    </section>
                  )
                })}
              </div>
            ) : !loadingVersions ? (
              <Card>
                <span>{text.noVersion}</span>
                <Button variant="primary" size="sm" onClick={() => { void onRefreshDshCatalog() }}>{text.loadVersions}</Button>
              </Card>
            ) : null}
          </>
        )}

        {tab === 'plugins' && (
          <>
            <Toolbar>
              <Input value={source} placeholder={text.extensionSource} onChange={(event) => { setSource(event.target.value) }} />
              <Button variant="secondary" onClick={() => { void browse() }}>{text.browseArchive}</Button>
              <Button variant="primary" disabled={!source.trim()} onClick={() => { void addPlugin() }}>{text.addExtension}</Button>
            </Toolbar>
            <div className="extension-list plugin-repo-list">
              {plugins.length ? plugins.map((entry) => (
                <article key={entry.id} className="extension-row">
                  <div>
                    <strong>{entry.name}</strong>
                    <p>{entry.description ?? entry.diagnostic ?? ''}</p>
                  </div>
                  <div className="plugin-repo-actions">
                    <Badge variant="primary">{entry.kind}</Badge>
                    <code>{entry.version ?? '—'}</code>
                    {isGithub(entry.source) && <Badge variant="primary">{text.githubOnly}</Badge>}
                    {(references[entry.id] ?? 0) > 0 && <Badge variant="neutral">{text.usedByContainers(references[entry.id] ?? 0)}</Badge>}
                    <Button variant="secondary" size="sm" onClick={() => { void onExportPlugin(entry) }}>{text.exportPlugin}</Button>
                    <Button variant="danger" size="sm" onClick={() => { requestDeletePlugin(entry) }}>{text.remove}</Button>
                  </div>
                </article>
              )) : <p className="empty-extension">{text.noRepositoryPlugins}</p>}
            </div>
          </>
        )}

        {tab === 'bundles' && (
          <>
            <Card>
              <div className="ui-stack gap-4">
                <Toolbar>
                  <Input value={bundleName} placeholder={text.bundleName} onChange={(event) => { setBundleName(event.target.value) }} />
                  <Button variant="primary" size="lg" disabled={!bundleName.trim() || selected.length === 0} onClick={() => { void createBundle() }}>{text.createBundle}</Button>
                </Toolbar>
                {plugins.length > 0 ? (
                  <div className="ui-stack gap-2">
                    <p className="ui-field-help">{text.pickEntries}</p>
                    {plugins.map((entry) => (
                      <Checkbox
                        key={entry.id}
                        checked={selected.includes(entry.id)}
                        onChange={(event) => { toggleSelected(entry.id, event.target.checked) }}
                        label={<span className="ui-inline gap-2"><strong>{entry.name}</strong><code>{entry.version ?? '—'}</code></span>}
                      />
                    ))}
                  </div>
                ) : <p className="ui-field-help">{text.noRepositoryPlugins}</p>}
              </div>
            </Card>
            <Card>
              <div className="ui-stack gap-4">
                <p className="ui-field-label">{text.importBundle}</p>
                <Toolbar>
                  <Input value={bundleArchive} placeholder={text.importBundle} onChange={(event) => { setBundleArchive(event.target.value) }} />
                  <Button variant="secondary" onClick={() => { void browseBundleArchive() }}>{text.browseArchive}</Button>
                  <Select
                    value={conflict}
                    onChange={(event) => { setConflict(event.target.value) }}
                    options={[{ value: 'keep', label: text.conflictKeep }, { value: 'overwrite', label: text.conflictOverwrite }]}
                  />
                  <Button variant="primary" disabled={!bundleArchive.trim()} onClick={() => { void importBundle() }}>{text.importBundle}</Button>
                </Toolbar>
              </div>
            </Card>
            {bundles.length ? (
              <div className="ui-stack gap-3">
                {bundles.map((bundle) => (
                  <Card key={bundle.id} padding="sm">
                    <div className="ui-toolbar justify-between">
                      <div className="ui-stack gap-2">
                        <strong>{bundle.name}</strong>
                        <span className="ui-field-help">{text.bundleExtensionCount(bundle.entries.length)} · {formatSize(totalSize(bundle))}</span>
                      </div>
                      <div className="ui-toolbar">
                        <Button variant="secondary" size="sm" onClick={() => { void onExportBundle(bundle, 'quick') }}>{text.quickExport}</Button>
                        <Button variant="secondary" size="sm" onClick={() => { void onExportBundle(bundle, 'full') }}>{text.fullExport}</Button>
                        <Button variant="danger" size="sm" onClick={() => { requestDeleteBundle(bundle) }}>{text.remove}</Button>
                      </div>
                    </div>
                  </Card>
                ))}
              </div>
            ) : <Card><span>{text.noBundles}</span></Card>}
          </>
        )}

        {tab === 'template' && (
          <>
            <Card>
              <p className="ui-field-label">{text.buildScript}</p>
              <Toolbar>
                <Input value={scriptPath} placeholder={text.scriptPath} readOnly />
                <Button variant="secondary" onClick={() => { void browseScript() }}>{text.chooseScript}</Button>
                <Button variant="primary" disabled={!scriptPath.trim()} onClick={() => { void doBuild() }}>{text.buildTemplate}</Button>
              </Toolbar>
            </Card>
            <Card>
              <p className="ui-field-label">{text.importTemplate}</p>
              <Toolbar>
                <Button variant="secondary" onClick={() => { void importTemplateArchive() }}>{text.browseArchive}</Button>
              </Toolbar>
            </Card>
            <h2 className="template-list-title">{text.templateName}</h2>
            {templates.length > 0 ? (
              <div className="version-list template-list">
                {templates.map((template) => (
                  <section key={template.name} className="version-row">
                    <div className="harness-info">
                      <code>{template.name}</code>
                      <p className="harness-ref"><span className="label">{text.templateHarness}</span> <code>{template.harnessRef ?? '-'}</code> · {template.profile}</p>
                    </div>
                    <div className="version-actions">
                      <Button variant="secondary" size="sm" onClick={() => { void exportTemplateArchive(template.name) }}>{text.exportTemplate}</Button>
                      <Button variant="danger" size="sm" onClick={() => { requestDeleteTemplate(template.name) }}>{text.removeTemplate}</Button>
                    </div>
                  </section>
                ))}
              </div>
            ) : <Card><span>{text.noTemplate}</span></Card>}
            {scriptPreview ? (
              <Card padding="sm">
                <div className="ui-stack gap-3">
                  <div className="image-preview-header"><strong>{text.scriptPreviewTitle}</strong><span>{scriptPreview.name} · {scriptPreview.version} · {scriptPreview.harnessUrl} · {scriptPreview.profile}</span></div>
                  <div className="image-preview-ops"><span className="label">{text.operations}</span>
                    {scriptPreview.ops.map((op, i) => <div key={i} className="image-preview-op"><code>{i + 1}. ADD {op.kind}</code><span>{op.source} (line {op.line})</span></div>)}
                  </div>
                </div>
              </Card>
            ) : <p className="empty-extension">{text.noScriptPreview}</p>}
          </>
        )}

      </Tabs>

      <Dialog
        open={pendingTemplateDelete !== null}
        title={text.confirmTitle}
        description={pendingTemplateDelete === null ? '' : text.deleteTemplateConfirm(pendingTemplateDelete.name)}
        onClose={() => { setPendingTemplateDelete(null) }}
      >
        <div className="ui-dialog-actions">
          <Button variant="secondary" size="sm" onClick={() => { setPendingTemplateDelete(null) }}>{text.dialogCancel}</Button>
          <Button variant="danger" size="sm" onClick={() => { void confirmTemplateDelete() }}>{text.dialogConfirm}</Button>
        </div>
      </Dialog>

      <Dialog
        open={pendingDelete !== null}
        title={text.confirmTitle}
        description={pendingDelete === null ? '' : pendingDelete.kind === 'bundle'
          ? text.deleteBundleConfirm(pendingDelete.name)
          : pendingDelete.refs.length > 0
            ? text.deletePluginWithRefs(pendingDelete.name, pendingDelete.refs.join(', '))
            : text.deletePluginConfirm(pendingDelete.name)}
        onClose={() => { setPendingDelete(null) }}
      >
        <div className="ui-dialog-actions">
          <Button variant="secondary" size="sm" onClick={() => { setPendingDelete(null) }}>{text.dialogCancel}</Button>
          <Button variant="danger" size="sm" onClick={() => { void confirmDelete() }}>{text.dialogConfirm}</Button>
        </div>
      </Dialog>
    </section>
  )
}
