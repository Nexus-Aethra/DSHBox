import { useEffect, useState } from 'react'
import { boxApi } from './shared/api/box-api'
import type { BoxConfig, ContainerExtensions, DshContainer, DshVersion, Language, ServerServiceStatus, TaskRecord, ToolchainStatus } from './shared/types/domain'
import { ContainerDetails } from './features/container-details/ContainerDetails'
import { TaskPanel } from './features/tasks/TaskPanel'
import { ToolchainRow } from './features/toolchains/ToolchainRow'
import { LanguageSwitch } from './shared/ui/LanguageSwitch'
import { DirectoryCard, Workspace } from './shared/ui/Workspace'
type Section = 'versions' | 'container' | 'settings'
type SettingsPane = 'general' | 'toolchains'

const INITIAL_CONFIG: BoxConfig = { runtimeDirectory: null, selectedDshVersion: null, language: 'en', toolchainSources: {} }

const COPY = {
  en: {
    versions: 'DSH Version Code', container: 'DSH Container', settings: 'Settings',
    chooseTitle: 'Choose DSH runtime directory', welcome: 'Set up DSH Box', welcomeNote: 'Choose a local folder for DSH Box data.',
    chooseDirectory: 'Choose folder', runtimeDirectory: 'Runtime directory', changeDirectory: 'Change directory',
    toolchainTitle: 'Bundled runtime', toolchainNote: 'DSH Box includes a private Node, npm, and pnpm runtime.', managed: 'Included with DSH Box', notFound: 'Unavailable', refresh: 'Refresh',
    versionTitle: 'DSH Version Code', versionNote: 'Versions are loaded from deepseek-ai/deepseek-harness.', noVersion: 'No DSH version installed', addVersion: 'Add version', install: 'Install', installed: 'Installed', uninstall: 'Uninstall', loadVersions: 'Load versions', installing: 'Installing…',
    containerTitle: 'DSH Container', notConfigured: 'Not configured', language: 'Language', storage: 'Local storage', toolchainSettings: 'Runtime', general: 'General', saved: 'Saved', service: 'Background service', restartService: 'Restart service', serviceRunning: 'Running', serviceStopped: 'Not running',
    installedVersions: 'DSH version', containerName: 'Container name', namePlaceholder: 'My DSH workspace', containerProfile: 'Profile', profilePlaceholder: 'web', createContainer: 'Create container', creating: 'Creating…', noInstalledVersion: 'Install a DSH version first.',
    containers: 'Containers', start: 'Start', stop: 'Stop', open: 'Open', moreActions: 'More actions', rebuild: 'Rebuild', remove: 'Delete', running: 'Running', stopped: 'Stopped',
    containerDetails: 'Container details', back: 'Back', activeProfile: 'Active profile', profiles: 'Profiles', addProfile: 'Add profile', plugins: 'Plugins', skills: 'Skills', noPlugins: 'No enabled plugins in this profile.', noSkills: 'No container skills.', containerSkill: 'Container Skill', diagnostics: 'Diagnostics', version: 'DSH version', path: 'Path',
    tasks: 'Tasks', taskRunning: 'running', recentTasks: 'Recent', cancel: 'Cancel', retry: 'Retry', viewLog: 'View log', close: 'Close',
  },
  'zh-CN': {
    versions: 'DSH 版本代码', container: 'DSH 容器', settings: '设置',
    chooseTitle: '选择 DSH 运行目录', welcome: '设置 DSH Box', welcomeNote: '选择一个本地文件夹来存储 DSH Box 数据。',
    chooseDirectory: '选择文件夹', runtimeDirectory: '运行目录', changeDirectory: '更改目录',
    toolchainTitle: '内置运行时', toolchainNote: 'DSH Box 已内置私有的 Node、npm 与 pnpm 运行时。', managed: '随 DSH Box 提供', notFound: '不可用', refresh: '刷新',
    versionTitle: 'DSH 版本代码', versionNote: '版本直接从 deepseek-ai/deepseek-harness 获取。', noVersion: '尚未安装 DSH 版本', addVersion: '添加版本', install: '安装', installed: '已安装', uninstall: '卸载', loadVersions: '获取版本', installing: '正在安装…',
    containerTitle: 'DSH 容器', notConfigured: '尚未配置', language: '语言', storage: '本地存储', toolchainSettings: '运行时', general: '通用', saved: '已保存', service: '后台服务', restartService: '重启服务', serviceRunning: '运行中', serviceStopped: '未运行',
    installedVersions: 'DSH 版本', containerName: '容器名称', namePlaceholder: '我的 DSH 工作区', containerProfile: 'Profile', profilePlaceholder: 'web', createContainer: '创建容器', creating: '正在创建…', noInstalledVersion: '请先安装一个 DSH 版本。',
    containers: '容器列表', start: '启动', stop: '停止', open: '进入使用', moreActions: '更多操作', rebuild: '重新构建', remove: '删除', running: '运行中', stopped: '已停止',
    containerDetails: 'Container 详情', back: '返回', activeProfile: '当前 Profile', profiles: 'Profiles', addProfile: '新增 Profile', plugins: '插件', skills: '技能', noPlugins: '这个 Profile 没有启用插件。', noSkills: '没有 Container 专属 Skill。', containerSkill: 'Container Skill', diagnostics: '诊断信息', version: 'DSH 版本', path: '路径',
    tasks: '任务', taskRunning: '进行中', recentTasks: '最近任务', cancel: '取消', retry: '重试', viewLog: '查看日志', close: '关闭',
  },
} as const

export function App() {
  const [config, setConfig] = useState<BoxConfig>(INITIAL_CONFIG)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [section, setSection] = useState<Section>('container')
  const [settingsPane, setSettingsPane] = useState<SettingsPane>('general')
  const [toolchains, setToolchains] = useState<ToolchainStatus[]>([])
  const [detecting, setDetecting] = useState(false)
  const [expandedToolchain, setExpandedToolchain] = useState<string | null>(null)
  const [dshVersions, setDshVersions] = useState<DshVersion[]>([])
  const [loadingVersions, setLoadingVersions] = useState(false)
  const [installingVersion, setInstallingVersion] = useState<string | null>(null)
  const [installedDshVersions, setInstalledDshVersions] = useState<string[]>([])
  const [containerVersion, setContainerVersion] = useState<string>('')
  const [creatingContainer, setCreatingContainer] = useState(false)
  const [creatingContainerView, setCreatingContainerView] = useState(false)
  const [containerName, setContainerName] = useState('')
  const [containerProfile, setNewContainerProfile] = useState('web')
  const [containers, setContainers] = useState<DshContainer[]>([])
  const [selectedContainer, setSelectedContainer] = useState<DshContainer | null>(null)
  const [containerDetails, setContainerDetails] = useState<ContainerExtensions | null>(null)
  const [containerMenuId, setContainerMenuId] = useState<string | null>(null)
  const [tasks, setTasks] = useState<TaskRecord[]>([])
  const [taskPanelOpen, setTaskPanelOpen] = useState(false)
  const [taskLog, setTaskLog] = useState<{ id: string; content: string } | null>(null)
  const [serverService, setServerService] = useState<ServerServiceStatus | null>(null)
  const text = COPY[config.language]
  const activeTaskFor = (resourceKey: string) => tasks.find((task) => task.resourceKeys.includes(resourceKey) && (task.status === 'queued' || task.status === 'running'))

  useEffect(() => {
    void boxApi.loadConfig().then(setConfig).catch((reason: unknown) => { setError(String(reason)) }).finally(() => { setLoading(false) })
    void refreshToolchains()
    void boxApi.getServerServiceStatus().then(setServerService).catch(() => undefined)
  }, [])

  useEffect(() => {
    void boxApi.listTasks().then(setTasks).catch(() => undefined)
    const update = (task: TaskRecord) => setTasks((current) => [task, ...current.filter((item) => item.id !== task.id)])
    const refreshForTask = (task: TaskRecord) => {
      if (task.status !== 'succeeded') return
      if (task.kind === 'dsh-version-install' || task.kind === 'dsh-catalog-refresh') { void loadDshVersions(); void loadInstalledDshVersions() }
      if (task.kind.startsWith('container-')) void loadContainers()
    }
    const taskEvents = ['task://created', 'task://updated', 'task://finished'].map((event) => boxApi.listenTask<TaskRecord>(event, (payload) => { update(payload); refreshForTask(payload) }))
    const logEvent = boxApi.listenTask<{ taskId: string; line: string }>('task://log', (payload) => setTaskLog((current) => current?.id === payload.taskId ? { ...current, content: `${current.content}${current.content ? '\n' : ''}${payload.line}` } : current))
    const unlisteners = Promise.all([...taskEvents, logEvent])
    return () => { void unlisteners.then((items) => items.forEach((unlisten) => unlisten())) }
  }, [])

  async function refreshToolchains(): Promise<void> {
    setDetecting(true)
    try { setToolchains(await boxApi.detectToolchains()) } catch (reason) { setError(String(reason)) } finally { setDetecting(false) }
  }

  async function loadDshVersions(): Promise<void> {
    setLoadingVersions(true)
    try { setDshVersions(await boxApi.listDshVersions()); setError(null) } catch (reason) { setError(String(reason)) } finally { setLoadingVersions(false) }
  }

  async function installDshVersion(version: string): Promise<void> {
    setInstallingVersion(version)
    try { await boxApi.enqueueDshVersionInstall(version); setError(null) } catch (reason) { setError(String(reason)) } finally { setInstallingVersion(null) }
  }

  async function refreshDshCatalog(): Promise<void> {
    try { await boxApi.enqueueDshCatalogRefresh(); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function uninstallDshVersion(version: string): Promise<void> {
    if (!window.confirm(`Uninstall DSH ${version}?`)) return
    try { setConfig(await boxApi.uninstallDshVersion(version)); await loadDshVersions(); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function loadInstalledDshVersions(): Promise<void> {
    try { const versions = await boxApi.listInstalledDshVersions(); setInstalledDshVersions(versions); setContainerVersion((current) => current || versions[0] || '') } catch (reason) { setError(String(reason)) }
  }

  async function createContainer(): Promise<void> {
    if (!containerVersion) return
    setCreatingContainer(true)
    try { await boxApi.createContainer(containerName, containerVersion, containerProfile); await loadContainers(); setCreatingContainerView(false); setContainerName(''); setNewContainerProfile('web'); setError(null) } catch (reason) { setError(String(reason)) } finally { setCreatingContainer(false) }
  }

  async function loadContainers(): Promise<void> {
    try { setContainers(await boxApi.listContainers()) } catch (reason) { setError(String(reason)) }
  }

  async function showContainerDetails(container: DshContainer): Promise<void> {
    setSelectedContainer(container); setContainerDetails(null)
    try { setContainerDetails(await boxApi.getContainerDetails(container.id)); setError(null) } catch (reason) { setError(String(reason)) }
  }
  async function refreshSelectedContainer(id: string): Promise<void> {
    const [updated, details] = await Promise.all([boxApi.listContainers(), boxApi.getContainerDetails(id)])
    setContainers(updated); setSelectedContainer(updated.find((item) => item.id === id) ?? null); setContainerDetails(details)
  }
  async function addContainerProfile(profile: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.addContainerProfile(selectedContainer.id, profile); await refreshSelectedContainer(selectedContainer.id); setError(null) } catch (reason) { setError(String(reason)) }
  }
  async function setContainerProfile(profile: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.setContainerProfile(selectedContainer.id, profile); await refreshSelectedContainer(selectedContainer.id); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function toggleContainer(container: DshContainer): Promise<void> {
    try {
      if (container.status === 'running') await boxApi.enqueueContainerStop(container.id)
      else await boxApi.enqueueContainerStart(container.id)
      setError(null)
    } catch (reason) { setError(String(reason)) }
  }

  async function openContainer(container: DshContainer): Promise<void> {
    try { await boxApi.openContainer(container.id); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function rebuildContainer(container: DshContainer): Promise<void> {
    try { await boxApi.enqueueContainerRebuild(container.id); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function deleteContainer(container: DshContainer): Promise<void> {
    if (!window.confirm(`Delete container ${container.id}?`)) return
    try { await boxApi.deleteContainer(container.id); await loadContainers(); setContainerMenuId(null); setError(null) } catch (reason) { setError(String(reason)) }
  }

  async function chooseRuntimeDirectory(): Promise<void> {
    try {
      const selected = await boxApi.chooseDirectory(text.chooseTitle)
      if (selected === null || Array.isArray(selected)) return
      setConfig(await boxApi.saveRuntimeDirectory(selected))
      setError(null)
    } catch (reason) { setError(String(reason)) }
  }

  async function changeLanguage(language: Language): Promise<void> {
    try {
      setConfig(await boxApi.saveLanguage(language))
      setError(null)
    } catch (reason) { setError(String(reason)) }
  }
  async function restartServerService(): Promise<void> { try { await boxApi.restartServerService(); setServerService(await boxApi.getServerServiceStatus()); setError(null) } catch (reason) { setError(String(reason)) } }

  async function cancelTask(id: string): Promise<void> { try { await boxApi.cancelTask(id); setError(null) } catch (reason) { setError(String(reason)) } }
  async function retryTask(id: string): Promise<void> { try { await boxApi.retryTask(id); setError(null) } catch (reason) { setError(String(reason)) } }
  async function showTaskLog(id: string): Promise<void> { try { setTaskLog({ id, content: await boxApi.readTaskLog(id) }); setTaskPanelOpen(true) } catch (reason) { setError(String(reason)) } }

  if (loading) return <main className="app-shell" />

  if (config.runtimeDirectory === null) {
    return <main className="app-shell onboarding-shell">
      <header className="onboarding-top"><div className="brand">DSH Box</div><LanguageSwitch language={config.language} onChange={changeLanguage} /></header>
      <section className="onboarding">
        <p className="eyebrow">WELCOME</p><h1>{text.welcome}</h1><p className="workspace-note">{text.welcomeNote}</p>
        <button type="button" className="primary large-primary" onClick={() => { void chooseRuntimeDirectory() }}>{text.chooseDirectory}</button>
        {error !== null && <p className="error" role="alert">{error}</p>}
      </section>
    </main>
  }

  return <main className="app-shell">
    <header className="topbar">
      <div className="brand">DSH Box</div>
      <nav className="navigation" aria-label="Workspace sections">
        {(['versions', 'container'] as const).map((item) => <button key={item} type="button" className={item === section ? 'nav-item active' : 'nav-item'} onClick={() => { setSection(item); if (item === 'versions') { void loadDshVersions(); void refreshDshCatalog() } if (item === 'container') { void loadInstalledDshVersions(); void loadContainers() } }}>{text[item]}</button>)}
      </nav>
      <div className="topbar-actions"><button type="button" className="task-button" onClick={() => { setTaskPanelOpen(true) }}>{text.tasks}{tasks.filter((task) => task.status === 'queued' || task.status === 'running').length > 0 && <span>{tasks.filter((task) => task.status === 'queued' || task.status === 'running').length}</span>}</button><button type="button" className={section === 'settings' ? 'settings-button active' : 'settings-button'} onClick={() => { setSection('settings') }}>{text.settings}</button></div>
    </header>
    {section === 'versions' && <section className="workspace"><div className="workspace-heading"><div><p className="eyebrow">VERSIONS</p><h1>{text.versionTitle}</h1></div><button type="button" className="secondary" onClick={() => { void refreshDshCatalog() }}>{tasks.some((task) => task.kind === 'dsh-catalog-refresh' && (task.status === 'queued' || task.status === 'running')) ? '…' : text.loadVersions}</button></div><p className="workspace-note">{text.versionNote}</p>{dshVersions.length > 0 && <div className="version-list">{dshVersions.map((version) => { const task = activeTaskFor(`runtime:${version.name}`); return <section key={version.name} className="version-row"><code>{version.name}</code>{version.installed ? <div className="version-actions"><span className="installed">{text.installed}</span><button type="button" className="secondary" disabled={task !== undefined} onClick={() => { void uninstallDshVersion(version.name) }}>{text.uninstall}</button></div> : <button type="button" className="primary" disabled={task !== undefined || installingVersion !== null} onClick={() => { void installDshVersion(version.name) }}>{task !== undefined || installingVersion === version.name ? text.installing : text.install}</button>}</section> })}</div>}{dshVersions.length === 0 && !loadingVersions && <section className="card empty-card"><span>{text.noVersion}</span><button type="button" className="primary" onClick={() => { void refreshDshCatalog() }}>{text.loadVersions}</button></section>}{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'container' && selectedContainer !== null && <ContainerDetails container={selectedContainer} details={containerDetails} text={text} onBack={() => { setSelectedContainer(null); setContainerDetails(null) }} onAddProfile={addContainerProfile} onSelectProfile={setContainerProfile} />}
    {section === 'container' && selectedContainer === null && !creatingContainerView && <section className="workspace"><div className="workspace-heading"><div><p className="eyebrow">CONTAINER</p><h1>{text.containerTitle}</h1></div><button type="button" className="create-icon" aria-label={text.createContainer} title={text.createContainer} onClick={() => { setCreatingContainerView(true) }}>+</button></div><div className="version-list container-list">{containers.map((container) => { const task = activeTaskFor(`container:${container.id}`); const busy = task !== undefined; const running = container.status === 'running'; return <section key={container.id} className="version-row"><button type="button" className="container-summary" onClick={() => { void showContainerDetails(container) }}><strong>{container.name}</strong><p className="container-meta">{container.version} · {container.profile} · {busy ? task.stage : running ? text.running : text.stopped}</p></button><div className="container-actions"><button type="button" className={running ? 'icon-action running' : 'icon-action'} aria-label={running ? text.stop : text.start} title={running ? text.stop : text.start} disabled={busy} onClick={() => { void toggleContainer(container) }}>{running ? <span aria-hidden="true" className="pause-icon" /> : <span aria-hidden="true" className="play-icon" />}</button><button type="button" className="icon-action" aria-label={text.open} title={text.open} disabled={!running || busy} onClick={() => { void openContainer(container) }}><span aria-hidden="true" className="open-icon">↗</span></button><div className="container-menu-wrap"><button type="button" className="icon-action" aria-label={text.moreActions} title={text.moreActions} disabled={busy} aria-expanded={containerMenuId === container.id} onClick={() => { setContainerMenuId((current) => current === container.id ? null : container.id) }}><span aria-hidden="true" className="more-icon">•••</span></button>{containerMenuId === container.id && <div className="container-menu" role="menu"><button type="button" role="menuitem" onClick={() => { setContainerMenuId(null); void rebuildContainer(container) }}>{text.rebuild}</button><button type="button" role="menuitem" className="danger-menu-item" onClick={() => { void deleteContainer(container) }}>{text.remove}</button></div>}</div></div></section> })}</div>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'container' && selectedContainer === null && creatingContainerView && <section className="workspace create-container-view"><button type="button" className="back-button" onClick={() => { setCreatingContainerView(false) }}>←</button><p className="eyebrow">CONTAINER</p><h1>{text.createContainer}</h1><section className="create-form"><label><span>{text.containerName}</span><input value={containerName} placeholder={text.namePlaceholder} onChange={(event) => { setContainerName(event.target.value) }} autoFocus /></label><label><span>{text.containerProfile}</span><input value={containerProfile} placeholder={text.profilePlaceholder} onChange={(event) => { setNewContainerProfile(event.target.value) }} required /></label><label><span>{text.installedVersions}</span>{installedDshVersions.length > 0 ? <select value={containerVersion} onChange={(event) => { setContainerVersion(event.target.value) }}>{installedDshVersions.map((version) => <option key={version} value={version}>{version}</option>)}</select> : <p className="field-help">{text.noInstalledVersion}</p>}</label><button type="button" className="primary" disabled={!containerName.trim() || !containerProfile.trim() || !containerVersion || creatingContainer} onClick={() => { void createContainer() }}>{creatingContainer ? text.creating : text.createContainer}</button></section>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {section === 'settings' && <section className="workspace"><p className="eyebrow">SETTINGS</p><h1>{text.settings}</h1><div className="settings-layout"><nav className="settings-sidebar" aria-label={text.settings}><button type="button" className={settingsPane === 'general' ? 'active' : ''} onClick={() => { setSettingsPane('general') }}>{text.general}</button><button type="button" className={settingsPane === 'toolchains' ? 'active' : ''} onClick={() => { setSettingsPane('toolchains'); void refreshToolchains() }}>{text.toolchainSettings}</button></nav><div className="settings-content">{settingsPane === 'general' ? <div className="settings-list"><section className="settings-row"><div><p className="label">{text.storage}</p><p className="path">{config.runtimeDirectory}</p></div><button type="button" className="secondary" onClick={() => { void chooseRuntimeDirectory() }}>{text.changeDirectory}</button></section><section className="settings-row"><div><p className="label">{text.service}</p><p className="path">{serverService?.supported ? `${serverService.running ? text.serviceRunning : text.serviceStopped} · ${serverService.detail}` : serverService?.detail ?? text.notConfigured}</p></div>{serverService?.supported && <button type="button" className="secondary" onClick={() => { void restartServerService() }}>{text.restartService}</button>}</section><section className="settings-row"><p className="label solo-label">{text.language}</p><LanguageSwitch language={config.language} onChange={changeLanguage} /></section></div> : <section className="runtime-settings"><div className="workspace-heading"><div><p className="eyebrow">RUNTIME</p><h2>{text.toolchainTitle}</h2></div><button type="button" className="secondary" onClick={() => { void refreshToolchains() }}>{detecting ? '…' : text.refresh}</button></div><p className="workspace-note">{text.toolchainNote}</p><div className="toolchain-list">{toolchains.map((toolchain) => <ToolchainRow key={toolchain.id} toolchain={toolchain} runtimeLabel={text.managed} notFound={text.notFound} expanded={expandedToolchain === toolchain.id} onToggle={() => { setExpandedToolchain(expandedToolchain === toolchain.id ? null : toolchain.id) }} />)}</div></section>}</div></div>{error !== null && <p className="error" role="alert">{error}</p>}</section>}
    {taskPanelOpen && <TaskPanel tasks={tasks} log={taskLog} text={text} onClose={() => { setTaskPanelOpen(false); setTaskLog(null) }} onCancel={cancelTask} onRetry={retryTask} onLog={showTaskLog} />}
  </main>
}
