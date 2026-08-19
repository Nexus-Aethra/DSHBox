import { useEffect, useState } from 'react'
import { boxApi } from './shared/api/box-api'
import type { Language } from './shared/types/domain'
import { pickText } from './i18n'
import { NPM_REGISTRY_PRESETS, useSettings } from './state/useSettings'
import { useContainers } from './state/useContainers'
import { useResources } from './state/useResources'
import { useTasks } from './state/useTasks'
import { ContainerDetails } from './features/container-details/ContainerDetails'
import { ResourcesPage } from './features/resources-page/ResourcesPage'
import { TaskPanel } from './features/tasks/TaskPanel'
import { ToolchainRow } from './features/toolchains/ToolchainRow'
import { LanguageSwitch } from './shared/ui/LanguageSwitch'
import { DirectoryCard, Workspace } from './shared/ui/Workspace'
import { Button } from './ui/Button'
import { Card } from './ui/Card'
import { Field } from './ui/Field'
import { Input } from './ui/Input'
import { Select } from './ui/Select'
import { Toolbar } from './ui/Toolbar'
import { Stack } from './ui/Stack'
type Section = 'container' | 'resources' | 'settings'
type SettingsPane = 'general' | 'toolchains'

/**
 * Startup gate: shows a spinner while the daemon comes up (fresh spawn or
 * the build-batch restart after an upgrade) and only mounts the real UI once
 * it answers `ping`. The data hooks inside `MainApp` therefore always run
 * against a ready daemon and their initial loads succeed.
 */
export function App() {
  const [ready, setReady] = useState(false)
  const [failed, setFailed] = useState<string | null>(null)
  const [language, setLanguage] = useState<Language>('en')
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    let cancelled = false
    let timer: number | undefined
    async function waitForDaemon(): Promise<void> {
      setFailed(null)
      try {
        const config = await boxApi.loadConfig()
        if (cancelled) return
        setLanguage(config.language)
        const deadline = Date.now() + 30_000
        while (Date.now() < deadline) {
          if (cancelled) return
          try {
            if (await boxApi.getDaemonStatus()) {
              setReady(true)
              return
            }
          } catch {
            // Daemon not reachable yet; keep polling.
          }
          await new Promise<void>((resolve) => { timer = window.setTimeout(resolve, 800) })
        }
        if (!cancelled) setFailed('dshboxd did not become ready within 30 seconds')
      } catch (reason) {
        if (!cancelled) setFailed(String(reason))
      }
    }
    void waitForDaemon()
    return () => {
      cancelled = true
      if (timer !== undefined) window.clearTimeout(timer)
    }
  }, [attempt])

  if (ready) return <MainApp />
  const text = pickText(language)
  return (
    <main className="app-shell startup-shell">
      <div className="startup" role="status">
        <span className="startup-spinner" aria-hidden="true" />
        <p>{failed ?? text.startingService}</p>
        {failed !== null && (
          <Button variant="primary" size="sm" onClick={() => { setAttempt((current) => current + 1) }}>{text.retry}</Button>
        )}
      </div>
    </main>
  )
}

export function MainApp() {
  const [error, setError] = useState<string | null>(null)
  const [section, setSection] = useState<Section>('resources')
  const [settingsPane, setSettingsPane] = useState<SettingsPane>('general')

  const settings = useSettings(setError)
  const { config, setConfig } = settings
  // Cross-hook wiring: the resources snapshot carries the container list and
  // the container details refresh the resources read model. The closures are
  // only invoked after every hook has been initialised, so the late-bound
  // `containers` reference below is safe.
  let containers!: ReturnType<typeof useContainers>
  const resources = useResources(setError, (items) => containers.setContainers(items))
  containers = useContainers(
    setError,
    (items) => resources.setPlugins(items),
  )
  const tasks = useTasks(
    {
      onVersionsChanged: () => { void settings.loadDshVersions(); void settings.loadInstalledDshVersions() },
      onTemplatesChanged: () => void containers.loadTemplates(),
      onContainersChanged: () => void containers.loadContainers(),
      onRepositoryChanged: () => void resources.loadPlugins(),
      onBundlesChanged: () => void resources.loadBundles(),
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

  async function chooseTemplateExportDestination(defaultName: string): Promise<string | null> {
    try { const selected = await boxApi.choosePluginExport(text.exportPlugin, defaultName); return typeof selected === 'string' ? selected : null } catch (reason) { setError(String(reason)); return null }
  }

  async function importTemplateArchive(archive: string): Promise<void> {
    try { await resources.importTemplateFromArchive(archive, null); await containers.loadTemplates(); } catch (reason) { setError(String(reason)) }
  }

  async function exportTemplateToPath(name: string, destination: string): Promise<void> {
    try { await resources.exportTemplateToFile(name, destination); } catch (reason) { setError(String(reason)) }
  }

  async function removeTemplateByName(name: string): Promise<void> {
    try {
      await resources.removeTemplateByName(name)
      // The template was deleted from the index; the derived harness list
      // and the container dropdown must both refresh, otherwise the user
      // can still submit the now-deleted name and the in-flight task can
      // still reference it.
      await Promise.all([containers.loadTemplates(), settings.loadDshVersions()])
    } catch (reason) { setError(String(reason)) }
  }

  // Wrap the install/uninstall handlers so the post-action refresh runs
  // for both stores: pulling or removing a harness mutates the template
  // index (canonical) and the Harness tab reads through it, so they
  // have to be kept in lockstep on this client.
  async function installDshVersion(version: string): Promise<void> {
    await settings.installDshVersion(version)
    await Promise.all([settings.loadDshVersions(), containers.loadTemplates()])
  }
  async function uninstallDshVersion(version: string): Promise<void> {
    await settings.uninstallDshVersion(version)
    await Promise.all([settings.loadDshVersions(), containers.loadTemplates()])
  }

  // Page-scoped data loading: every section fetches its own resources the
  // moment the user enters it (and only then). Extra requests compared to a
  // mount-time prefetch are cheap; staleness bugs (CLI pulled a template but
  // the UI list stayed empty) are structurally impossible.
  function loadSection(target: Section): void {
    if (target === 'resources') {
      void resources.loadPlugins()
      void resources.loadBundles()
      void settings.loadDshVersions()
      void settings.refreshDshCatalog()
      void containers.loadTemplates()
    }
    if (target === 'container') {
      void settings.loadInstalledDshVersions()
      void containers.loadContainers()
      void containers.loadTemplates()
    }
    if (target === 'settings') {
      void settings.refreshToolchains()
    }
  }

  // Load the default section once the startup gate clears (config loaded and
  // a runtime directory chosen); onboarding completion flips the dependency.
  const onboardingDone = !settings.loading && config.runtimeDirectory !== null
  useEffect(() => {
    if (onboardingDone) loadSection(section)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onboardingDone])

  if (settings.loading) {
    return (
      <main className="app-shell startup-shell">
        <div className="startup" role="status">
          <span className="startup-spinner" aria-hidden="true" />
          <p>{text.startingService}</p>
        </div>
      </main>
    )
  }

  if (config.runtimeDirectory === null) {
    return (
      <main className="app-shell onboarding-shell">
        <header className="onboarding-top">
          <div className="brand">DSH Box</div>
          <LanguageSwitch language={config.language} onChange={settings.changeLanguage} />
        </header>
        <section className="onboarding">
          <p className="eyebrow">WELCOME</p>
          <h1>{text.welcome}</h1>
          <p className="workspace-note">{text.welcomeNote}</p>
          <Button variant="primary" size="lg" onClick={() => { void settings.chooseRuntimeDirectory() }}>{text.chooseDirectory}</Button>
          {error !== null && <p className="error" role="alert">{error}</p>}
        </section>
      </main>
    )
  }

  return <main className="app-shell">
    <header className="topbar">
      <div className="brand">DSH Box</div>
      <nav className="navigation" aria-label="Workspace sections">
        {(['resources', 'container'] as const).map((item) => (
          <Button
            key={item}
            variant="ghost"
            size="sm"
            className={`nav-item${item === section ? ' active' : ''}`}
            onClick={() => {
              setSection(item)
              loadSection(item)
            }}
          >
            {text[item]}
          </Button>
        ))}
      </nav>
      <div className="topbar-actions">
        <button type="button" className="task-button" onClick={() => { tasks.setTaskPanelOpen(true) }}>{text.tasks}{tasks.tasks.filter((task) => task.status === 'queued' || task.status === 'running').length > 0 && <span>{tasks.tasks.filter((task) => task.status === 'queued' || task.status === 'running').length}</span>}</button>
        <Button variant={section === 'settings' ? 'ghost' : 'ghost'} size="sm" className={section === 'settings' ? 'settings-button active' : 'settings-button'} onClick={() => { setSection('settings'); loadSection('settings') }}>{text.settings}</Button>
      </div>
    </header>
    {section === 'container' && containers.selectedContainer !== null && <ContainerDetails container={containers.selectedContainer} details={containers.containerDetails} repository={resources.plugins} bundles={resources.bundles} workspaceExtensions={containers.workspaceExtensions} text={text} onBack={() => { containers.setSelectedContainer(null); containers.setContainerDetails(null); containers.setWorkspaceExtensions([]) }} onAddProfile={containers.addContainerProfile} onSelectProfile={containers.setContainerProfile} onAddExtension={containers.addContainerExtension} onAddBundle={async (profile, bundleId, conflict) => { if (containers.selectedContainer !== null) await resources.installBundle(containers.selectedContainer.id, profile, bundleId, conflict) }} onDeletePlugin={containers.deleteContainerPlugin} onScanWorkspace={containers.scanWorkspaceExtensions} onImportWorkspace={containers.importWorkspaceExtension} onReadLog={containers.readContainerLog} onOpenInBrowser={containers.openContainerInBrowser} />}
    {section === 'container' && containers.selectedContainer === null && !containers.creatingContainerView && (
      <section className="workspace">
        <div className="workspace-heading">
          <div><p className="eyebrow">CONTAINER</p><h1>{text.containerTitle}</h1></div>
          <Button variant="primary" shape="icon" size="lg" aria-label={text.createContainer} title={text.createContainer} onClick={() => { containers.setCreatingContainerView(true) }}>+</Button>
        </div>
        <div className="version-list container-list">
          {containers.containers.map((container) => {
            const task = activeTaskFor(`container:${container.id}`)
            const busy = task !== undefined
            const running = container.status === 'running'
            const corrupted = container.status === 'corrupted'
            const statusText = busy ? task.stage : (running ? text.running : corrupted ? text.corrupted : text.stopped)
            return (
              <section key={container.id} className="version-row">
                <button type="button" className="container-summary" onClick={() => { void containers.showContainerDetails(container) }}>
                  <strong>{container.name}</strong>
                  <p className="container-meta">{container.version} · {container.profile} · {statusText}</p>
                </button>
                <div className="container-actions">
                  <Button shape="icon" variant="ghost" className={running ? 'icon-action running' : 'icon-action'} aria-label={running ? text.stop : (corrupted ? text.rebuild : text.start)} title={running ? text.stop : (corrupted ? text.rebuild : text.start)} disabled={busy} onClick={() => { corrupted ? void containers.rebuildContainer(container) : void containers.toggleContainer(container) }}>
                    {running ? <span aria-hidden="true" className="pause-icon" /> : <span aria-hidden="true" className="play-icon" />}
                  </Button>
                  <Button shape="icon" variant="ghost" className="icon-action" aria-label={text.open} title={text.open} disabled={!running || busy || corrupted} onClick={() => { void containers.openContainer(container) }}>
                    <span aria-hidden="true" className="open-icon">↗</span>
                  </Button>
                  <div className="container-menu-wrap">
                    <Button shape="icon" variant="ghost" className="icon-action" aria-label={text.moreActions} title={text.moreActions} disabled={busy} aria-expanded={containers.containerMenuId === container.id} onClick={() => { containers.setContainerMenuId((current) => current === container.id ? null : container.id) }}>
                      <span aria-hidden="true" className="more-icon">•••</span>
                    </Button>
                    {containers.containerMenuId === container.id && (
                      <div className="container-menu" role="menu">
                        <button type="button" role="menuitem" className="container-menu-button" onClick={() => { containers.setContainerMenuId(null); void containers.rebuildContainer(container) }}>{text.rebuild}</button>
                        <button type="button" role="menuitem" className="container-menu-button danger-menu-item" onClick={() => { void containers.deleteContainer(container) }}>{text.remove}</button>
                      </div>
                    )}
                  </div>
                </div>
              </section>
            )
          })}
        </div>
        {error !== null && <p className="error" role="alert">{error}</p>}
      </section>
    )}
    {section === 'container' && containers.selectedContainer === null && containers.creatingContainerView && (
      <section className="workspace create-container-view">
        <button type="button" className="back-button" onClick={() => { containers.setCreatingContainerView(false) }}>←</button>
        <p className="eyebrow">CONTAINER</p>
        <h1>{text.createContainer}</h1>
        <Card>
          <Stack gap={5}>
            <Field label={text.containerName} required>
              {(id) => <Input id={id} value={containers.containerName} placeholder={text.namePlaceholder} onChange={(event) => { containers.setContainerName(event.target.value) }} autoFocus />}
            </Field>
            <Field label={text.template} required help={containers.templates.length === 0 ? text.noTemplate : undefined}>
              {(id) => containers.templates.length > 0 ? (
                <Select
                  id={id}
                  value={containers.selectedTemplate}
                  onChange={(event) => { containers.setSelectedTemplate(event.target.value) }}
                  options={containers.templates.map((template) => ({ value: template.name, label: `${template.name}${template.harnessRef ? ` (${template.harnessRef})` : ''}` }))}
                />
              ) : null}
            </Field>
            <Field label={text.containerProfile} required>
              {(id) => <Input id={id} value={containers.newContainerProfile} placeholder={text.profilePlaceholder} onChange={(event) => { containers.setNewContainerProfile(event.target.value) }} />}
            </Field>
            <div>
              <Button variant="primary" size="lg" loading={containers.creatingContainer} disabled={!containers.containerName.trim() || !containers.newContainerProfile.trim() || !containers.selectedTemplate} onClick={() => { void containers.createContainer() }}>{containers.creatingContainer ? text.creating : text.createContainer}</Button>
            </div>
          </Stack>
        </Card>
        {error !== null && <p className="error" role="alert">{error}</p>}
      </section>
    )}
    {section === 'settings' && (
      <section className="workspace">
        <p className="eyebrow">SETTINGS</p>
        <h1>{text.settings}</h1>
        <div className="settings-layout">
          <nav className="settings-sidebar" aria-label={text.settings}>
            <Button variant="ghost" size="sm" className={settingsPane === 'general' ? 'active' : ''} onClick={() => { setSettingsPane('general') }}>{text.general}</Button>
            <Button variant="ghost" size="sm" className={settingsPane === 'toolchains' ? 'active' : ''} onClick={() => { setSettingsPane('toolchains'); void settings.refreshToolchains() }}>{text.toolchainSettings}</Button>
          </nav>
          <div className="settings-content">
            {settingsPane === 'general' ? (
              <div className="settings-list">
                <section className="settings-row">
                  <div><p className="label">{text.storage}</p><p className="path">{config.runtimeDirectory}</p></div>
                  <Button variant="secondary" size="sm" onClick={() => { void settings.chooseRuntimeDirectory() }}>{text.changeDirectory}</Button>
                </section>
                <section className="settings-row">
                  <div><p className="label">{text.service}</p><p className="path">{settings.serverService?.supported ? `${settings.serverService.running ? text.serviceRunning : text.serviceStopped} · ${settings.serverService.detail}` : settings.serverService?.detail ?? text.notConfigured}</p></div>
                  {settings.serverService?.supported && <Button variant="secondary" size="sm" onClick={() => { void settings.restartServerService() }}>{text.restartService}</Button>}
                </section>
                <section className="settings-row">
                  <div><p className="label">{text.githubMirror}</p><p className="path">{text.githubMirrorNote}</p></div>
                  <div className="settings-controls">
                    <Input value={settings.githubMirror} placeholder="https://gh-proxy.com" onChange={(event) => { settings.setGithubMirror(event.target.value) }} className="settings-input" />
                    <Button variant="primary" size="sm" disabled={settings.savingMirror} onClick={() => { void settings.saveGithubMirror() }}>{settings.savingMirror ? '…' : text.saveMirror}</Button>
                  </div>
                </section>
                <section className="settings-row">
                  <div><p className="label">{text.npmRegistry}</p><p className="path">{text.npmRegistryNote}</p></div>
                  <div className="settings-controls">
                    <Select value={settings.npmRegistry} onChange={(event) => { settings.setNpmRegistry(event.target.value); void settings.saveNpmRegistry() }} options={NPM_REGISTRY_PRESETS.map((preset) => ({ value: preset.value, label: preset.label }))} />
                    {settings.npmRegistry === '__custom__' && <Input value={settings.npmRegistryCustom} placeholder="https://registry.example.com" onChange={(event) => { settings.setNpmRegistryCustom(event.target.value) }} onBlur={() => { void settings.saveNpmRegistry() }} className="settings-input" />}
                  </div>
                </section>
                <section className="settings-row">
                  <p className="label solo-label">{text.language}</p>
                  <LanguageSwitch language={config.language} onChange={settings.changeLanguage} />
                </section>
              </div>
            ) : (
              <section className="runtime-settings">
                <div className="workspace-heading">
                  <div><p className="eyebrow">RUNTIME</p><h2>{text.toolchainTitle}</h2></div>
                  <Button variant="secondary" size="sm" onClick={() => { void settings.refreshToolchains() }}>{settings.detecting ? '…' : text.refresh}</Button>
                </div>
                <p className="workspace-note">{text.toolchainNote}</p>
                <div className="toolchain-list">
                  {settings.toolchains.map((toolchain) => (
                    <ToolchainRow key={toolchain.id} toolchain={toolchain} runtimeLabel={text.managed} notFound={text.notFound} expanded={settings.expandedToolchain === toolchain.id} onToggle={() => { settings.setExpandedToolchain(settings.expandedToolchain === toolchain.id ? null : toolchain.id) }} />
                  ))}
                </div>
              </section>
            )}
          </div>
        </div>
        {error !== null && <p className="error" role="alert">{error}</p>}
      </section>
    )}
    {section === 'resources' && <ResourcesPage plugins={resources.plugins} bundles={resources.bundles} references={resources.references} dshVersions={settings.dshVersions} installedDshVersions={settings.installedDshVersions} installingVersion={settings.installingVersion} loadingVersions={settings.loadingVersions} upgradingResources={settings.upgradingResources} upgradeReport={settings.upgradeReport} activeTaskFor={activeTaskFor} templates={containers.templates} scriptPreview={resources.scriptPreview} text={text} onImportPlugin={resources.importPlugin} onChooseArchive={chooseExtensionArchive} onExportPlugin={async (entry) => { await resources.exportPlugin(entry, text) }} onDeletePlugin={resources.deletePlugin} onLoadBundles={resources.loadBundles} onCreateBundle={resources.createBundle} onDeleteBundle={resources.deleteBundle} onExportBundle={async (bundle, mode) => { await resources.exportBundle(bundle, mode, text) }} onImportBundle={resources.importBundle} onInstallDshVersion={installDshVersion} onUninstallDshVersion={uninstallDshVersion} onRefreshDshCatalog={settings.refreshDshCatalog} onUpgradeResources={settings.upgradeResources} onReloadPlugins={resources.loadPlugins} onLoadTemplates={containers.loadTemplates} onChooseScript={async () => resources.chooseScriptFile(text)} onBuildTemplate={resources.buildTemplate} onImportTemplate={importTemplateArchive} onChooseExportDestination={chooseTemplateExportDestination} onExportTemplate={exportTemplateToPath} onRemoveTemplate={removeTemplateByName} />}
    {tasks.taskPanelOpen && <TaskPanel tasks={tasks.tasks} logs={tasks.taskLogs} text={text} onClose={() => { tasks.setTaskPanelOpen(false) }} onCancel={tasks.cancelTask} onRetry={tasks.retryTask} onLog={tasks.showTaskLog} onDelete={tasks.deleteTask} />}
  </main>
}
