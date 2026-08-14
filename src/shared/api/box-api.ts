import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open, save } from '@tauri-apps/plugin-dialog'
import type { BoxConfig, ContainerExtensions, DshContainer, DshVersion, Language, ResourceSnapshot, ResourceState, ServerServiceStatus, TaskRecord, ToolchainStatus } from '../types/domain'

type ToolchainPayload = { id: string; name: string; managedVersion: string | null }

/** The sole frontend boundary to desktop IPC and native dialogs. */
export const boxApi = {
  loadConfig: () => invoke<BoxConfig>('load_config'),
  saveRuntimeDirectory: (directory: string) => invoke<BoxConfig>('save_runtime_directory', { directory }),
  saveLanguage: (language: Language) => invoke<BoxConfig>('save_language', { language }),
  getServerServiceStatus: () => invoke<ServerServiceStatus>('get_server_service_status'),
  restartServerService: () => invoke<void>('restart_server_service'),
  detectToolchains: async () => (await invoke<ToolchainPayload[]>('detect_toolchains')).map(({ id, name, managedVersion }) => ({ id, name, version: managedVersion })),
  listDshVersions: () => invoke<DshVersion[]>('list_dsh_versions'),
  enqueueDshCatalogRefresh: () => invoke<TaskRecord>('enqueue_dsh_catalog_refresh'),
  enqueueDshVersionInstall: (version: string) => invoke<TaskRecord>('enqueue_dsh_version_install', { version }),
  uninstallDshVersion: (version: string) => invoke<BoxConfig>('uninstall_dsh_version', { version }),
  listInstalledDshVersions: () => invoke<string[]>('list_installed_dsh_versions'),
  createContainer: (name: string, version: string, profile: string) => invoke<DshContainer>('create_dsh_container', { request: { name, version, profile } }),
  listContainers: () => invoke<DshContainer[]>('list_dsh_containers'),
  getContainerDetails: (id: string) => invoke<ContainerExtensions | null>('get_container_details', { id }),
  addContainerProfile: (id: string, profile: string) => invoke<DshContainer>('add_dsh_container_profile', { id, profile }),
  setContainerProfile: (id: string, profile: string) => invoke<DshContainer>('set_dsh_container_profile', { id, profile }),
  enqueueContainerExtensionAdd: (id: string, profile: string, source: string) => invoke<TaskRecord>('enqueue_container_extension_add', { request: { id, profile, source } }),
  enqueuePluginExport: (sourceContainerId: string, sourcePath: string, destination: string) => invoke<TaskRecord>('enqueue_plugin_export', { request: { sourceContainerId, sourcePath, destination } }),
  listResourceStates: () => invoke<ResourceSnapshot>('list_resource_states'),
  getResourceState: (key: string) => invoke<ResourceState | null>('get_resource_state', { key }),
  refreshResourceState: () => invoke<ResourceSnapshot>('refresh_resource_state'),
  deleteContainer: (id: string) => invoke<void>('delete_dsh_container', { id }),
  enqueueContainerStart: (id: string) => invoke<TaskRecord>('enqueue_container_start', { id }),
  enqueueContainerStop: (id: string) => invoke<TaskRecord>('enqueue_container_stop', { id }),
  enqueueContainerRebuild: (id: string) => invoke<TaskRecord>('enqueue_container_rebuild', { id }),
  openContainer: (id: string) => invoke<void>('open_dsh_front', { id }),
  listTasks: () => invoke<TaskRecord[]>('list_tasks'),
  cancelTask: (id: string) => invoke<void>('cancel_task', { id }),
  retryTask: (id: string) => invoke<TaskRecord>('retry_task', { id }),
  readTaskLog: (id: string) => invoke<string>('read_task_log', { id }),
  chooseDirectory: (title: string) => open({ directory: true, multiple: false, title }),
  chooseExtensionArchive: (title: string) => open({ multiple: false, title, filters: [{ name: 'Tar archives', extensions: ['tar', 'tgz', 'gz', 'xz'] }] }),
  choosePluginExport: (title: string, defaultPath: string) => save({ title, defaultPath, filters: [{ name: 'Tarball', extensions: ['tar.gz'] }] }),
  listenTask: <T>(event: string, listener: (payload: T) => void) => listen<T>(event, ({ payload }) => listener(payload)),
}
