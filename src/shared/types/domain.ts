export type Language = 'en' | 'zh-CN'
export type ToolchainStatus = { id: string; name: string; version: string | null }
export type DshVersion = { name: string; installed: boolean }
// Result of one base-template generation pass for a single installed DSH
// version. The harness tab in the UI is now a friendly alias for these
// templates; there is no separate harness resource type any more.
export type HarnessUpgradeReport = {
  version: string
  templatePath: string
  templateCreated: boolean
}
export type DshContainer = { id: string; name: string; version: string; profile: string; template?: string; directory: string; status: string }
// Local template surfaced by the daemon. Stored under
// `<runtime>/templates/<content-hash>/script.dsh` and looked up through
// the runtime's template index; `id` is the fnv1a64 hash and `name` is the
// user-facing alias used by `template rm/show/export`.
export type TemplateInfo = { name: string; id: string; harnessRef: string | null; profile: string; built?: boolean }
export type ExtensionPlugin = { name: string; version: string | null; description: string | null; path: string | null; diagnostic: string | null }
export type ProfileExtensions = { name: string; plugins: ExtensionPlugin[]; diagnostics: string[] }
export type ContainerSkill = { name: string; description: string | null; path: string; diagnostic: string | null }
export type ContainerExtensions = { containerId: string; profiles: ProfileExtensions[]; skills: ContainerSkill[]; diagnostics: string[]; scannedAt: number }
export type ExtensionKind = 'plugin' | 'skill'
export type RepositoryExtension = { id: string; kind: ExtensionKind; name: string; version: string | null; description: string | null; contentDigest: string; sourcePath: string; importedAt: number; diagnostic: string | null; source: string | null }
export type BundleEntry = { repositoryId: string; kind: ExtensionKind; name: string; version: string | null; source: string | null; size: number; diagnostic: string | null }
export type ExtensionBundle = { id: string; name: string; entries: BundleEntry[]; createdAt: number }
export type WorkspaceExtension = { kind: ExtensionKind; name: string; version: string | null; description: string | null; relativePath: string; contentDigest: string; diagnostic: string | null }
export type TaskStatus = 'queued' | 'running' | 'waiting_input' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted'
export type TaskRecord = { id: string; kind: string; resourceKeys: string[]; status: TaskStatus; stage: string; progress: number; createdAt: number; startedAt: number | null; finishedAt: number | null; logPath: string; error: string | null; params: Record<string, unknown>; cancelRequested: boolean }
export type BoxConfig = { runtimeDirectory: string | null; selectedDshVersion: string | null; language: Language; toolchainSources: Record<string, string>; githubMirror: string | null; npmRegistry: string | null }
export type ResourceKind = 'toolchain' | 'runtime' | 'container' | 'task' | 'template' | 'plugin' | 'bundle'
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
  // id -> composite reference count (containers + templates). Each owner
  // type is tracked separately so the UI badge can split "used by N
  // container(s)" and "referenced by M template(s)".
  repositoryReferences?: Record<string, { containers: number; templates: number }>
  tasks: TaskRecord[]
  resources: Record<string, ResourceState>
  scannedAt: number
  updatedAt: number
}

// One entry of the content-addressed data store (`<root>/data/<digest>/`).
export type DataEntry = {
  name: string
  digest: string
  importedAt: number
  source: string
}

// ---- dshimage types ----
export type PreviewSourceKind = 'github' | 'tarball' | 'localDir' | 'bareName'
export type PreviewSource = {
  type: PreviewSourceKind
  url?: string
  ref_?: string
  local?: boolean
  path?: string
  name?: string
  scope?: string | null
  version?: string | null
}
export type PreviewOp = {
  kind: string
  line: number
  source: string
  parsed: PreviewSource
}
export type PreviewScriptResult = {
  name: string
  version: string
  harnessUrl: string
  profile: string
  labels: Record<string, string>
  ops: PreviewOp[]
}
export type ImageBuildRequest = {
  scriptPath: string
  outputPath?: string | null
  containerName?: string | null
}
// Resolved add from a v6+ manifest
export type ResolvedAdd = {
  kind: string
  source: AddSource
  destination: string
  blob: string
  digest: string
}
export type AddSource = {
  type: string
  url?: string
  ref_?: string | null
  local?: boolean
  path?: string
  pluginName?: string
  relPath?: string
  containerId?: string | null
}
// Legacy: kept for backward compat
export type ImageEntry = {
  kind: ExtensionKind
  name: string
  version?: string | null
  source?: string | null
  digest?: string | null
  blob?: string | null
  bareName?: { name: string; scope?: string | null } | null
}
export type ImageContainer = {
  profile: string
  dshVersion: string
}
export type ImageManifest = {
  schemaVersion: number
  mediaType: string
  id: string
  name: string
  version: string
  createdAt: number
  container: ImageContainer
  labels: Record<string, string>
  entries: ImageEntry[]
}
