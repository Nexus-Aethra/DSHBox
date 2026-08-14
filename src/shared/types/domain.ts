export type Language = 'en' | 'zh-CN'
export type ToolchainStatus = { id: string; name: string; version: string | null }
export type DshVersion = { name: string; installed: boolean }
export type DshContainer = { id: string; name: string; version: string; profile: string; directory: string; status: string }
export type ExtensionPlugin = { name: string; version: string | null; description: string | null; path: string | null; diagnostic: string | null }
export type ProfileExtensions = { name: string; plugins: ExtensionPlugin[]; diagnostics: string[] }
export type ContainerSkill = { name: string; description: string | null; path: string; diagnostic: string | null }
export type ContainerExtensions = { containerId: string; profiles: ProfileExtensions[]; skills: ContainerSkill[]; diagnostics: string[]; scannedAt: number }
export type ExtensionKind = 'plugin' | 'skill'
export type RepositoryExtension = { id: string; kind: ExtensionKind; name: string; version: string | null; description: string | null; contentDigest: string; sourcePath: string; importedAt: number; diagnostic: string | null }
export type WorkspaceExtension = { kind: ExtensionKind; name: string; version: string | null; description: string | null; relativePath: string; contentDigest: string; diagnostic: string | null }
export type TaskStatus = 'queued' | 'running' | 'waiting_input' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted'
export type TaskRecord = { id: string; kind: string; resourceKeys: string[]; status: TaskStatus; stage: string; progress: number; createdAt: number; startedAt: number | null; finishedAt: number | null; logPath: string; error: string | null; params: Record<string, unknown>; cancelRequested: boolean }
export type BoxConfig = { runtimeDirectory: string | null; selectedDshVersion: string | null; language: Language; toolchainSources: Record<string, string> }
export type ResourceKind = 'toolchain' | 'runtime' | 'container' | 'task'
export type ResourceHealth = 'ready' | 'missing' | 'busy' | 'failed' | 'unknown'
export type ResourceState = { resourceKey: string; kind: ResourceKind; name: string; health: ResourceHealth; detail: string | null; progress: number | null }
export type ServerServiceStatus = { supported: boolean; enabled: boolean; running: boolean; detail: string }
export type ResourceSnapshot = {
  runtimeDirectory: string | null
  language: string
  selectedDshVersion: string | null
  toolchains: ToolchainStatus[]
  versions: DshVersion[]
  containers: DshContainer[]
  containerExtensions: Record<string, ContainerExtensions>
  extensionRepository: RepositoryExtension[]
  tasks: TaskRecord[]
  resources: Record<string, ResourceState>
  scannedAt: number
  updatedAt: number
}
