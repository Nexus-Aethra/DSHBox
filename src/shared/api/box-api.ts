import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open, save } from '@tauri-apps/plugin-dialog'
import type { BoxConfig, ContainerExtensions, DataEntry, DshContainer, DshVersion, ExtensionBundle, GraphSourceKind, Language, PluginGraph, PreviewScriptResult, RepositoryReferenceRow, ResourceSnapshot, ResourceState, ServerServiceStatus, TaskRecord, TemplateInfo, ToolchainStatus, WorkspaceExtension } from '../types/domain'

type ToolchainPayload = { id: string; name: string; managedVersion: string | null }

type BridgeFrame = { ok: boolean; result?: unknown; error?: string }

// `pnpm dev` serves this frontend without a Tauri host, where `invoke` has no
// bridge to call and the startup gate would never see a daemon. In that case
// requests go to the dev server's `/__rpc` route, which forwards them to the
// running daemon; see `scripts/dev-rpc-bridge.mjs` for what it does and does not
// support. The packaged app always takes the Tauri branch, and `DEV` is false
// there, so the fallback is compiled out rather than shipped as dead code.
const hasDesktopBridge = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
const useDevBridge = import.meta.env.DEV && !hasDesktopBridge

async function ipc<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  if (!useDevBridge) return invoke<T>(command, payload)
  const response = await fetch('/__rpc', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ command, payload: payload ?? {} }),
  })
  const frame = (await response.json()) as BridgeFrame
  if (!frame.ok) throw new Error(frame.error ?? `${command} failed`)
  return frame.result as T
}

/** Native dialogs live in the desktop shell; fail clearly when they cannot. */
function nativeDialog<T>(run: () => Promise<T>): Promise<T> {
  if (hasDesktopBridge) return run()
  return Promise.reject(new Error('native file dialogs are not available in browser dev mode'))
}

/** The sole frontend boundary to desktop IPC and native dialogs. */
export const boxApi = {
  loadConfig: () => ipc<BoxConfig>('load_config'),
  saveRuntimeDirectory: (directory: string) => ipc<BoxConfig>('save_runtime_directory', { directory }),
  saveLanguage: (language: Language) => ipc<BoxConfig>('save_language', { language }),
  saveMirrorSettings: (githubMirror: string | null, npmRegistry: string | null) => ipc<BoxConfig>('save_mirror_settings', { githubMirror, npmRegistry }),
  getServerServiceStatus: () => ipc<ServerServiceStatus>('get_server_service_status'),
  getDaemonStatus: () => ipc<boolean>('get_daemon_status'),
  restartServerService: () => ipc<void>('restart_server_service'),
  detectToolchains: async () => (await ipc<ToolchainPayload[]>('detect_toolchains')).map(({ id, name, managedVersion }) => ({ id, name, version: managedVersion })),
  listDshVersions: () => ipc<DshVersion[]>('list_dsh_versions'),
  enqueueDshCatalogRefresh: () => ipc<TaskRecord>('enqueue_dsh_catalog_refresh'),
  uninstallDshVersion: (version: string) => ipc<BoxConfig>('uninstall_dsh_version', { version }),
  listInstalledDshVersions: () => ipc<string[]>('list_installed_dsh_versions'),
  pullTemplate: (version: string) => ipc<TaskRecord>('enqueue_pull_template', { version }),
  openDshFrontBrowser: (id: string) => ipc<void>('open_dsh_front_browser', { id }),
  upgradeLegacyResources: () => ipc<string[]>('upgrade_legacy_resources'),
  createContainer: (name: string, version: string, profile: string) => ipc<DshContainer>('create_dsh_container', { request: { name, version, profile } }),
    listTemplates: () => ipc<TemplateInfo[]>('list_templates'),
    readTemplate: (name: string) => ipc<{ name: string; text: string }>('read_template', { name }),
    importTemplate: (archive: string, name: string | null) => ipc<string>('import_template', { request: { archive, name } }),
    exportTemplate: (name: string, destination: string | null) => ipc<string>('export_template', { request: { name, destination } }),
    removeTemplate: (name: string) => ipc<string>('remove_template', { request: { name } }),
    createContainerFromTemplate: (name: string, template: string, profile: string | null) => ipc<TaskRecord>('enqueue_template_container', { request: { name, template, profile } }),
  listContainers: () => ipc<DshContainer[]>('list_dsh_containers'),
  getContainerDetails: (id: string) => ipc<ContainerExtensions | null>('get_container_details', { id }),
  addContainerProfile: (id: string, profile: string) => ipc<DshContainer>('add_dsh_container_profile', { id, profile }),
  setContainerProfile: (id: string, profile: string) => ipc<DshContainer>('set_dsh_container_profile', { id, profile }),
  enqueueContainerExtensionAdd: (id: string, profile: string, source: string) => ipc<TaskRecord>('enqueue_container_extension_add', { request: { id, profile, source } }),
  enqueueRepositoryExtensionImport: (source: string) => ipc<TaskRecord>('enqueue_repository_extension_import', { request: { source } }),
  scanContainerWorkspaceExtensions: (id: string) => ipc<WorkspaceExtension[]>('scan_container_workspace_extensions', { id }),
  enqueueWorkspaceExtensionImport: (id: string, relativePath: string) => ipc<TaskRecord>('enqueue_workspace_extension_import', { request: { id, relativePath } }),
  enqueueContainerExtensionCopy: (id: string, profile: string | null, repositoryId: string) => ipc<TaskRecord>('enqueue_container_extension_copy', { request: { id, profile, repositoryId } }),
  enqueueRepositoryExtensionExport: (repositoryId: string, destination: string) => ipc<TaskRecord>('enqueue_repository_extension_export', { request: { repositoryId, destination } }),
  enqueueBundleExport: (id: string, destination: string, mode: string) => ipc<TaskRecord>('enqueue_bundle_export', { id, destination, mode }),
  enqueueBundleImport: (archive: string, conflict: string) => ipc<TaskRecord>('enqueue_bundle_import', { request: { archive, conflict } }),
  enqueueContainerBundleInstall: (id: string, profile: string, bundleId: string, conflict: string) => ipc<TaskRecord>('enqueue_container_bundle_install', { request: { id, profile, bundleId, conflict } }),
  listExtensionBundles: () => ipc<ExtensionBundle[]>('list_extension_bundles'),
  createExtensionBundle: (name: string, repositoryIds: string[]) => ipc<ExtensionBundle>('create_extension_bundle', { name, repositoryIds }),
  deleteExtensionBundle: (id: string) => ipc<void>('delete_extension_bundle', { id }),
  removeRepositoryExtension: (id: string) => ipc<void>('remove_repository_extension', { id }),
  enqueuePluginExport: (sourceContainerId: string, sourcePath: string, destination: string) => ipc<TaskRecord>('enqueue_plugin_export', { request: { sourceContainerId, sourcePath, destination } }),
  removeRepositoryPlugin: (id: string, profile: string, name: string) => ipc<void>('remove_repository_plugin', { id, profile, name }),
  listResourceStates: () => ipc<ResourceSnapshot>('list_resource_states'),
  listRepositoryReferenceCounts: () => ipc<RepositoryReferenceRow[]>('list_repository_reference_counts'),
  pluginDependencyGraph: (kind: GraphSourceKind, id: string) => ipc<PluginGraph>('plugin_dependency_graph', { kind, id }),
  getResourceState: (key: string) => ipc<ResourceState | null>('get_resource_state', { key }),
  refreshResourceState: () => ipc<ResourceSnapshot>('refresh_resource_state'),
  listDataEntries: () => ipc<DataEntry[]>('list_data_entries'),
  pruneOrphanedData: () => ipc<string[]>('prune_orphaned_data'),
  deleteContainer: (id: string) => ipc<void>('delete_dsh_container', { id }),
  enqueueContainerStart: (id: string) => ipc<TaskRecord>('enqueue_container_start', { id }),
  enqueueContainerStop: (id: string) => ipc<TaskRecord>('enqueue_container_stop', { id }),
  enqueueContainerRebuild: (id: string) => ipc<TaskRecord>('enqueue_container_rebuild', { id }),
  openContainer: (id: string) => ipc<void>('open_dsh_front', { id }),
  listTasks: () => ipc<TaskRecord[]>('list_tasks'),
  cancelTask: (id: string) => ipc<void>('cancel_task', { id }),
  deleteTask: (id: string) => ipc<void>('delete_task', { id }),
  retryTask: (id: string) => ipc<TaskRecord>('retry_task', { id }),
  readTaskLog: (id: string) => ipc<string>('read_task_log', { id }),
  readContainerLog: (id: string, log: 'host' | 'rebuild' | 'webview') => ipc<string>('read_container_log', { id, log }),
  chooseDirectory: (title: string) => nativeDialog(() => open({ directory: true, multiple: false, title })),
  chooseExtensionArchive: (title: string) => nativeDialog(() => open({ multiple: false, title, filters: [{ name: 'Tar archives', extensions: ['tar', 'tgz', 'gz', 'xz'] }] })),
  chooseScriptFile: (title: string) => nativeDialog(() => open({ multiple: false, title, filters: [{ name: 'DSH build scripts', extensions: ['dsh'] }] })),
  choosePluginExport: (title: string, defaultPath: string) => nativeDialog(() => save({ title, defaultPath, filters: [{ name: 'Tarball', extensions: ['tar.gz'] }] })),
  // Task progress normally arrives over the Tauri event bus. Without it the
  // three-second task poll in `useTasks` is the only source, so this degrades to
  // a no-op instead of throwing.
  listenTask: <T>(event: string, listener: (payload: T) => void) =>
    hasDesktopBridge
      ? listen<T>(event, ({ payload }) => listener(payload))
      : Promise.resolve(() => {}),
  // Image (dshimage) commands
  previewImageScript: (path: string) => ipc<PreviewScriptResult>('preview_image_script_command', { path }),
  enqueueImageBuild: (scriptPath: string, outputPath?: string | null, containerName?: string | null) => ipc<TaskRecord>('enqueue_image_build', { request: { scriptPath, outputPath, containerName } }),
  enqueueImageCommit: (containerId: string, outputPath: string, name: string, version: string) => ipc<TaskRecord>('enqueue_image_commit_stub', { request: { containerId, outputPath, name, version } }),
  enqueueImageLoad: (archivePath: string) => ipc<TaskRecord>('enqueue_image_load_stub', { request: { archivePath } }),
}
