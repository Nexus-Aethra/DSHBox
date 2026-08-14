import { useState } from 'react'
import { boxApi } from './shared/api/box-api'
import type { Language } from './shared/types/domain'
import { pickText } from './i18n'
import { NPM_REGISTRY_PRESETS, useSettings } from './state/useSettings'
import { useContainers } from './state/useContainers'
import { useRepository } from './state/useRepository'
import { useTasks } from './state/useTasks'
import { ContainerDetails } from './features/container-details/ContainerDetails'
import { PluginRepo } from './features/plugin-repo/PluginRepo'
import { TaskPanel } from './features/tasks/TaskPanel'
import { ToolchainRow } from './features/toolchains/ToolchainRow'
import { LanguageSwitch } from './shared/ui/LanguageSwitch'
import { DirectoryCard, Workspace } from './shared/ui/Workspace'
type Section = 'versions' | 'container' | 'pluginRepo' | 'settings'
type SettingsPane = 'general' | 'toolchains'

export function App() {
  const [error, setError] = useState<string | null>(null)
  const [section, setSection] = useState<Section>('container')
  const [settingsPane, setSettingsPane] = useState<SettingsPane>('general')

  const settings = useSettings(setError)
  const { config, setConfig } = settings
  // Cross-hook wiring: the repository snapshot carries the container list and
  // the container details refresh the repository read model. The closures are
  // only invoked after every hook has been initialised, so the late-bound
  // `containers` reference below is safe.
  let containers!: ReturnType<typeof useContainers>
  const repository = useRepository(setError, (items) => containers.setContainers(items))
  containers = useContainers(
    settings.installedDshVersions,
    setError,
    (items) => repository.setRepositoryDetails(items),
  )
  const tasks = useTasks(
    {
      onVersionsChanged: () => { void settings.loadDshVersions(); void settings.loadInstalledDshVersions() },
      onContainersChanged: () => void containers.loadContainers(),
      onRepositoryChanged: () => void repository.loadPluginRepository(),
      onBundlesChanged: () => void repository.loadBundles(),
      onContainerDetailsChanged: (containerId) => {
        if (containers.selectedContainer?.id === containerId) return containers.refreshSelectedContainer(containerId)
        return Promise.resolve()
      },
    },
    setError,
  )
  const text = pickText(config.language)
  const activeTaskFor = (resourceKey: string) => tasks.tasks.find((task) => task.resourceKeys.includes(resourceKey) && (task.status === 'queued' || task.status === 'running'))

  async function chooseExtensionArchive(): Promise<string | null> {
    try { const selected = await boxApi.chooseExtensionArchive(text.browseArchive); return typeof selected === 'string' ? selected : null } catch (reason) { setError(String(reason)); return null }
  }

  if (settings.loading) return <main className="app-shell" />

  if (config.runtimeDirectory === null) {
    return <main className="app-shell onboarding-shell">
      <header className="onboarding-top"><div className="brand">DSH Box</div><LanguageSwitch language={config.language} onChange={settings.changeLanguage} /></header>
      <section className="onboarding">
        <p className="eyebrow">WELCOME</p><h1>{text.welcome}</h1><p className="workspace-note">{text.welcomeNote}</p>
        <button type="button" className="primary large-primary" onClick={() => { void settings.chooseRuntimeDirectory() }}>{text.chooseDirectory}</button>
        {error !== null && <p className="error" role="alert">{error}</p>}
      </section>
    </main>
  }

  return <main className="app-shell">
    <header className="topbar">
      <div className="brand">DSH Box</div>
      <nav className="navigation" aria-label="Workspace sections">
        {(['versions', 'container', 'pluginRepo'] as const).map((item) => <button key={item} type="button" className={item === section ? 'nav-item active' : 'nav-item'} onClick={() => { setSection(item); if (item === 'versions') { void settings.loadDshVersions(); void settings.refreshDshCatalog() } if (item === 'container') { void settings.loadInstalledDshVersions(); void containers.loadContainers() } if (item === 'pluginRepo') void repository.loadPluginRepository() }}>{text[item]}</button>)}
      </nav>
      <div className="topbar-actions"><button type="button" className="task-button" onClick={() => { tasks.setTaskPanelOpen(true) }}>{text.tasks}{tasks.tasks.filter((task) => task.status === 'queued' || task.status === 'running').length > 0 && <span>{tasks.tasks.filter((task) => task.status === 'queued' || task.status === 'running').length}</span>}</button><button type="button" className={section === 'settings' ? 'settings-button active' : 'settings-button'} onClick={() => { setSection('settings') }}>{text.settings}</button></div>
    </header>
    {section === 'versions' && <section className="workspace"><div className="workspace-heading"><div><p className="eyebrow">VERSIONS</p><h1>{text.versionTitle}</h1></div><button type="button" className="secondary" onClick={() => { void settings.refreshDshCatalog() }}>{tasks.tasks.some((task) => task.kind === 'dsh-catalog-refresh' && (task.status === 'queued' || task.status === 'running')) ? '…' : text.loadVersions}</button></div><p className="workspace-note">{text.versionNote}</p>{settings.dshVersions.length > 0 && <div className="version-list">{settings.dshVersions.map((version) => { const task = activeTaskFor(`runtime:${version.name}`); return <section key={version.name} className="version-row"><code>{version.name}</code>{version.installed ? <div className="version-actions"><span className="installed">{text.installed}</span><button type="button" className="secondary" disabled={task !== undefined} onClick={() => { void settings.uninstallDshVersion(version.name) }}>{text.uninstall}</button></div> : <button type="button" className="primary" disabled={task !== undefined || settings.installingVersion !== null} onClick={() => { void settings.installDshVersion(version.name) }}>{task !== undefined || settings.installingVersion === version.name ? text.installing : text.install}</button>}</section> })}</div>}{settings.dshVersions.length === 0 && !settings.loadingVersions && <section className="card empty-card"><span>{text.noVersion}</span><button type="button" className="primary" onClick={() => { void settings.refreshDshCatalog() }}>{text.loadVersions}</button></section>}{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'container' && containers.selectedContainer !== null && <ContainerDetails container={containers.selectedContainer} details={containers.containerDetails} repository={repository.repositoryDetails} bundles={repository.bundles} workspaceExtensions={containers.workspaceExtensions} text={text} onBack={() => { containers.setSelectedContainer(null); containers.setContainerDetails(null); containers.setWorkspaceExtensions([]) }} onAddProfile={containers.addContainerProfile} onSelectProfile={containers.setContainerProfile} onAddExtension={containers.addContainerExtension} onAddBundle={async (profile, bundleId, conflict) => { if (containers.selectedContainer !== null) await repository.installBundle(containers.selectedContainer.id, profile, bundleId, conflict) }} onDeletePlugin={containers.deleteContainerPlugin} onScanWorkspace={containers.scanWorkspaceExtensions} onImportWorkspace={containers.importWorkspaceExtension} onReadLog={containers.readContainerLog} onOpenInBrowser={containers.openContainerInBrowser} />}
    {section === 'container' && containers.selectedContainer === null && !containers.creatingContainerView && <section className="workspace"><div className="workspace-heading"><div><p className="eyebrow">CONTAINER</p><h1>{text.containerTitle}</h1></div><button type="button" className="create-icon" aria-label={text.createContainer} title={text.createContainer} onClick={() => { containers.setCreatingContainerView(true) }}>+</button></div><div className="version-list container-list">{containers.containers.map((container) => { const task = activeTaskFor(`container:${container.id}`); const busy = task !== undefined; const running = container.status === 'running'; return <section key={container.id} className="version-row"><button type="button" className="container-summary" onClick={() => { void containers.showContainerDetails(container) }}><strong>{container.name}</strong><p className="container-meta">{container.version} · {container.profile} · {busy ? task.stage : running ? text.running : text.stopped}</p></button><div className="container-actions"><button type="button" className={running ? 'icon-action running' : 'icon-action'} aria-label={running ? text.stop : text.start} title={running ? text.stop : text.start} disabled={busy} onClick={() => { void containers.toggleContainer(container) }}>{running ? <span aria-hidden="true" className="pause-icon" /> : <span aria-hidden="true" className="play-icon" />}</button><button type="button" className="icon-action" aria-label={text.open} title={text.open} disabled={!running || busy} onClick={() => { void containers.openContainer(container) }}><span aria-hidden="true" className="open-icon">↗</span></button><div className="container-menu-wrap"><button type="button" className="icon-action" aria-label={text.moreActions} title={text.moreActions} disabled={busy} aria-expanded={containers.containerMenuId === container.id} onClick={() => { containers.setContainerMenuId((current) => current === container.id ? null : container.id) }}><span aria-hidden="true" className="more-icon">•••</span></button>{containers.containerMenuId === container.id && <div className="container-menu" role="menu"><button type="button" role="menuitem" onClick={() => { containers.setContainerMenuId(null); void containers.rebuildContainer(container) }}>{text.rebuild}</button><button type="button" role="menuitem" className="danger-menu-item" onClick={() => { void containers.deleteContainer(container) }}>{text.remove}</button></div>}</div></div></section> })}</div>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'container' && containers.selectedContainer === null && containers.creatingContainerView && <section className="workspace create-container-view"><button type="button" className="back-button" onClick={() => { containers.setCreatingContainerView(false) }}>←</button><p className="eyebrow">CONTAINER</p><h1>{text.createContainer}</h1><section className="create-form"><label><span>{text.containerName}</span><input value={containers.containerName} placeholder={text.namePlaceholder} onChange={(event) => { containers.setContainerName(event.target.value) }} autoFocus /></label><label><span>{text.containerProfile}</span><input value={containers.newContainerProfile} placeholder={text.profilePlaceholder} onChange={(event) => { containers.setNewContainerProfile(event.target.value) }} required /></label><label><span>{text.installedVersions}</span>{settings.installedDshVersions.length > 0 ? <select value={containers.containerVersion} onChange={(event) => { containers.setContainerVersion(event.target.value) }}>{settings.installedDshVersions.map((version) => <option key={version} value={version}>{version}</option>)}</select> : <p className="field-help">{text.noInstalledVersion}</p>}</label><button type="button" className="primary" disabled={!containers.containerName.trim() || !containers.newContainerProfile.trim() || !containers.containerVersion || containers.creatingContainer} onClick={() => { void containers.createContainer() }}>{containers.creatingContainer ? text.creating : text.createContainer}</button></section>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'settings' && <section className="workspace"><p className="eyebrow">SETTINGS</p><h1>{text.settings}</h1><div className="settings-layout"><nav className="settings-sidebar" aria-label={text.settings}><button type="button" className={settingsPane === 'general' ? 'active' : ''} onClick={() => { setSettingsPane('general') }}>{text.general}</button><button type="button" className={settingsPane === 'toolchains' ? 'active' : ''} onClick={() => { setSettingsPane('toolchains'); void settings.refreshToolchains() }}>{text.toolchainSettings}</button></nav><div className="settings-content">{settingsPane === 'general' ? <div className="settings-list"><section className="settings-row"><div><p className="label">{text.storage}</p><p className="path">{config.runtimeDirectory}</p></div><button type="button" className="secondary" onClick={() => { void settings.chooseRuntimeDirectory() }}>{text.changeDirectory}</button></section><section className="settings-row"><div><p className="label">{text.service}</p><p className="path">{settings.serverService?.supported ? `${settings.serverService.running ? text.serviceRunning : text.serviceStopped} · ${settings.serverService.detail}` : settings.serverService?.detail ?? text.notConfigured}</p></div>{settings.serverService?.supported && <button type="button" className="secondary" onClick={() => { void settings.restartServerService() }}>{text.restartService}</button>}</section><section className="settings-row"><div><p className="label">{text.githubMirror}</p><p className="path">{text.githubMirrorNote}</p></div><input className="settings-input" value={settings.githubMirror} placeholder="https://gh-proxy.com" onChange={(event) => { settings.setGithubMirror(event.target.value) }} /></section><section className="settings-row"><div><p className="label">{text.npmRegistry}</p><p className="path">{text.npmRegistryNote}</p></div><div className="settings-controls"><select value={settings.npmRegistry} onChange={(event) => { settings.setNpmRegistry(event.target.value) }}>{NPM_REGISTRY_PRESETS.map((preset) => <option key={preset.value} value={preset.value}>{preset.label}</option>)}</select>{settings.npmRegistry === '__custom__' && <input className="settings-input" value={settings.npmRegistryCustom} placeholder="https://registry.example.com" onChange={(event) => { settings.setNpmRegistryCustom(event.target.value) }} />}</div></section><button type="button" className="primary settings-save" disabled={settings.savingMirror} onClick={() => { void settings.saveMirrorSettings() }}>{settings.savingMirror ? '…' : text.saveMirror}</button><section className="settings-row"><p className="label solo-label">{text.language}</p><LanguageSwitch language={config.language} onChange={settings.changeLanguage} /></section></div> : <section className="runtime-settings"><div className="workspace-heading"><div><p className="eyebrow">RUNTIME</p><h2>{text.toolchainTitle}</h2></div><button type="button" className="secondary" onClick={() => { void settings.refreshToolchains() }}>{settings.detecting ? '…' : text.refresh}</button></div><p className="workspace-note">{text.toolchainNote}</p><div className="toolchain-list">{settings.toolchains.map((toolchain) => <ToolchainRow key={toolchain.id} toolchain={toolchain} runtimeLabel={text.managed} notFound={text.notFound} expanded={settings.expandedToolchain === toolchain.id} onToggle={() => { settings.setExpandedToolchain(settings.expandedToolchain === toolchain.id ? null : toolchain.id) }} />)}</div></section>}</div></div>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'pluginRepo' && <PluginRepo entries={repository.repositoryDetails} bundles={repository.bundles} text={text} onImport={repository.importRepositoryPlugin} onChooseArchive={chooseExtensionArchive} onExport={async (entry) => { await repository.exportRepositoryPlugin(entry, text) }} onDelete={repository.deleteRepositoryPlugin} onLoadBundles={repository.loadBundles} onCreateBundle={repository.createBundle} onDeleteBundle={repository.deleteBundle} onExportBundle={async (bundle, mode) => { await repository.exportBundle(bundle, mode, text) }} onImportBundle={repository.importBundle} />}
    {tasks.taskPanelOpen && <TaskPanel tasks={tasks.tasks} logs={tasks.taskLogs} text={text} onClose={() => { tasks.setTaskPanelOpen(false) }} onCancel={tasks.cancelTask} onRetry={tasks.retryTask} onLog={tasks.showTaskLog} onDelete={tasks.deleteTask} />}
  </main>
}
