// Dev-only RPC bridge: lets `pnpm dev` serve the Box UI in a plain browser.
//
// `pnpm tauri dev` loads this same frontend from the Vite server, but inside a
// webview where `invoke` reaches the Rust command layer. A plain browser has no
// such bridge, so the startup gate never sees a daemon and the UI stops at the
// recovery screen. This plugin adds the missing hop for the read-only surface:
// the frontend posts `{ command, payload }` to `/__rpc`, and the middleware
// forwards it to the running `dshboxd` over its loopback RPC.
//
// It deliberately does NOT try to emulate the whole desktop layer. Commands that
// are desktop-orchestrated rather than daemon-backed — anything that enqueues a
// scheduler task, drives the process lifecycle, or opens a native dialog — answer
// with an explicit error instead of a plausible-looking stub, so a missing
// capability is obvious rather than silently wrong. Use the packaged app or the
// CLI for those.
//
// The proxy exists because the browser cannot call the daemon directly: the
// daemon answers on a dynamic loopback port with no CORS headers, and giving a
// privileged local daemon a permissive CORS policy to serve a debug convenience
// would be the wrong trade. Proxying keeps the browser on one origin.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, isAbsolute, join, resolve } from 'node:path'

const CONFIG_DIR = process.env.DSHBOX_CONFIG_DIR ?? join(homedir(), '.dsh-box')
const CONFIG_PATH = join(CONFIG_DIR, 'config.json')
const DISCOVERY_PATH = join(CONFIG_DIR, 'server', 'discovery.json')

// The two halves of `box_dsh_versions::HARNESS_STANDARD_REF`, which a version tag
// is appended to: `pull_template` takes a full `<repo>:<ref>` string.
const HARNESS_REPO = 'github.com/deepseek-ai/deepseek-harness'

/// The desktop layer resolves a user-supplied path against its own working
/// directory before handing it to the daemon, and leaves refs and URLs alone.
/// Mirrored here so a relative path typed into the dev UI means the same thing
/// it would in the packaged app. See `absolutize_path` in
/// `src-tauri/src/desktop/app/rpc.rs`.
function absolute(path) {
  const trimmed = String(path ?? '').trim()
  if (
    trimmed.startsWith('http://') ||
    trimmed.startsWith('https://') ||
    trimmed.startsWith('github.com/') ||
    isAbsolute(trimmed)
  ) {
    return trimmed
  }
  return resolve(trimmed)
}

const DEFAULT_CONFIG = {
  runtimeDirectory: null,
  selectedDshVersion: null,
  language: 'en',
  toolchainSources: {},
  githubMirror: null,
  npmRegistry: null,
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'))
  } catch {
    return null
  }
}

function readConfig() {
  return { ...DEFAULT_CONFIG, ...(readJson(CONFIG_PATH) ?? {}) }
}

/// Merge into the existing file rather than replacing it: the Rust side owns
/// other fields (`pluginsManifestDigest`, schema bookkeeping) that a browser
/// round-trip must not drop.
function writeConfig(patch) {
  const merged = { ...readConfig(), ...patch }
  mkdirSync(dirname(CONFIG_PATH), { recursive: true })
  writeFileSync(CONFIG_PATH, JSON.stringify(merged, null, 2))
  return merged
}

/// Forward one call to the daemon. The daemon takes its auth token in the body,
/// which is why this is a POST with a JSON envelope rather than a plain proxy.
async function callDaemon(method, params) {
  const discovery = readJson(DISCOVERY_PATH)
  if (!discovery?.port || !discovery?.token) {
    throw new Error('dshboxd is not running (no discovery record); start it first')
  }
  const response = await fetch(`http://127.0.0.1:${discovery.port}/rpc`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token: discovery.token, method, ...params }),
  })
  const frame = await response.json()
  if (!frame.ok) throw new Error(frame.error ?? `${method} failed`)
  // Async daemon methods answer with `task` instead of `result`; both are the
  // caller's payload.
  return frame.result ?? frame.task ?? null
}

/// `list_repository_reference_counts` returns rows with owner id lists, while the
/// snapshot carries projected counts. This mirrors the projection the desktop
/// read model performs in `box-state`.
function projectReferences(rows) {
  const projected = {}
  for (const row of rows ?? []) {
    projected[row.id] = {
      containers: (row.containers ?? []).length,
      templates: (row.templates ?? []).length,
    }
  }
  return projected
}

// The bridge implements part of the command surface, not all of it. An unlisted
// command used to be reported as "orchestrated by the desktop layer, not the
// daemon", which was wrong often enough to mislead: `enqueue_container_start` is
// a plain daemon call, and the CLI starts containers exactly that way. Some
// omissions really do need the desktop process — native dialogs, a webview
// window per container, OS service management — and the rest are simply not
// wired up yet.
const NOT_WIRED = (command) =>
  new Error(
    `${command} is not wired into the dev bridge (it answers ${Object.keys(COMMANDS).length} of the UI's commands). ` +
      'Add it to COMMANDS if the daemon backs it; commands that need the desktop process — native dialogs, the per-container webview window, OS service management — can only run in the packaged app. ' +
      'The CLI covers both cases meanwhile.',
  )

/// Commands the bridge can answer, as `command -> (params) => Promise<result>`.
const COMMANDS = {
  // ── startup gate and shell ────────────────────────────────────────────────
  get_daemon_status: async () => {
    try {
      await callDaemon('ping', {})
      return true
    } catch {
      return false
    }
  },
  load_config: async () => readConfig(),
  save_language: async ({ language }) => writeConfig({ language }),
  save_runtime_directory: async ({ directory }) => writeConfig({ runtimeDirectory: directory }),
  save_mirror_settings: async ({ githubMirror, npmRegistry }) =>
    writeConfig({ githubMirror, npmRegistry }),
  get_server_service_status: async () => ({
    supported: false,
    enabled: false,
    running: false,
    detail: 'not applicable in browser dev mode',
  }),
  get_resource_state: async () => null,

  // ── reads that map straight onto a daemon method ──────────────────────────
  list_templates: ({}) => callDaemon('list_templates', {}),
  read_template: ({ name }) => callDaemon('read_template', { name }),
  list_extension_bundles: () => callDaemon('list_bundles', {}),
  list_repository_reference_counts: () => callDaemon('list_repository_reference_counts', {}),
  list_dsh_containers: () => callDaemon('list_containers', {}),
  list_tasks: () => callDaemon('list_tasks', {}),
  list_data_entries: () => callDaemon('list_data_entries', {}),
  detect_toolchains: () => callDaemon('detect_toolchains', {}),
  list_installed_dsh_versions: () => callDaemon('list_installed_dsh_versions', {}),
  plugin_dependency_graph: ({ kind, id }) => callDaemon('plugin_dependency_graph', { kind, id }),
  list_container_resources: ({ id, plugin }) => callDaemon('list_container_resources', { id, plugin }),
  list_resources: () => callDaemon('list_resources', {}),
  browse_container_paths: ({ id, path }) => callDaemon('browse_container_paths', { id, path }),
  enqueue_resource_extract: (request) => callDaemon('enqueue_resource_extract', request),
  enqueue_resource_inject: (request) => callDaemon('enqueue_resource_inject', request),
  delete_resource: ({ resourceId }) => callDaemon('delete_resource', { resourceId }),

  // Not a projection of the installed names: the daemon already answers with the
  // full derived catalog, `{ name, installed }` per row, which is the shape the
  // UI wants. Deriving it from `list_installed_dsh_versions` instead left the
  // Resources list showing only what was already installed — on a fresh runtime,
  // just `latest`. This mirrors the desktop command in
  // `src-tauri/src/desktop/app/commands/versions.rs`.
  list_dsh_versions: () => callDaemon('list_dsh_catalog', {}),

  // The UI reads `ContainerExtensions`, which the daemon nests inside the
  // container description.
  get_container_details: async ({ id }) => {
    const description = await callDaemon('describe_container', { id })
    return description?.extensions ?? null
  },

  // The desktop projects its own read model here. Composing it from daemon reads
  // gives the pages something coherent to render; fields the desktop derives
  // locally (toolchains, per-resource health) stay empty rather than invented.
  list_resource_states: async () => {
    const config = readConfig()
    const [templates, containers, repository, references, tasks] = await Promise.all([
      callDaemon('list_templates', {}),
      callDaemon('list_containers', {}),
      callDaemon('list_repository_extensions', {}),
      callDaemon('list_repository_reference_counts', {}),
      callDaemon('list_tasks', {}),
    ]).catch(() => [null, null, null, null, null])
    const now = Math.floor(Date.now() / 1000)
    return {
      runtimeDirectory: config.runtimeDirectory,
      language: config.language,
      selectedDshVersion: config.selectedDshVersion,
      toolchains: [],
      versions: [],
      containers: containers ?? [],
      containerExtensions: {},
      extensionRepository: repository ?? [],
      repositoryReferences: projectReferences(references),
      tasks: tasks ?? [],
      resources: {},
      scannedAt: now,
      updatedAt: now,
    }
  },

  // ── writes that are a single daemon call ──────────────────────────────────
  cancel_task: ({ id }) => callDaemon('cancel_task', { id }),
  delete_task: ({ id }) => callDaemon('delete_task', { id }),
  remove_template: ({ request }) => callDaemon('remove_template', { name: request?.name }),
  remove_repository_extension: ({ id }) => callDaemon('remove_repository_extension', { id }),
  delete_dsh_container: ({ id }) => callDaemon('delete_container', { id }),
  delete_extension_bundle: ({ id }) => callDaemon('delete_extension_bundle', { id }),
  prune_orphaned_data: () => callDaemon('prune_orphaned_data', {}),
  upgrade_legacy_resources: () => callDaemon('upgrade_legacy_resources', {}),
  enqueue_dsh_catalog_refresh: () => callDaemon('refresh_dsh_catalog', {}),

  // ── container lifecycle ───────────────────────────────────────────────────
  // The daemon owns the host process, so these are daemon calls outright: the
  // desktop commands for them are passthroughs with unused `_manager`/`_app`
  // parameters, and `dshbox container start` reaches the daemon the same way.
  // Task progress arrives through the 3s `list_tasks` poll.
  enqueue_container_start: ({ id }) => callDaemon('enqueue_container_start', { id }),
  enqueue_container_stop: ({ id }) => callDaemon('enqueue_container_stop', { id }),
  enqueue_container_rebuild: ({ id }) => callDaemon('enqueue_container_rebuild', { id }),
  create_dsh_container: ({ request }) =>
    callDaemon('create_container', { name: request.name, version: request.version, profile: request.profile }),
  enqueue_template_container: ({ request }) =>
    callDaemon('create_container_from_template', {
      name: request.name,
      template: request.template,
      profile: request.profile,
    }),

  // ── extensions, bundles and templates ─────────────────────────────────────
  // Each of these is one daemon call. The desktop commands that wrap them add
  // `is_safe_identifier` / path guards, which is not reproduced here, and that
  // difference is visible: the daemon queues most of these before it looks at the
  // arguments, so a bad id or a missing file surfaces as a failed task instead of
  // an immediate error. The UI only offers values it has already listed, so this
  // costs a developer nothing; it is worth knowing before reading a task list.
  // The desktop also rebuilds its read model after these writes, which the UI
  // stands in for by refetching the affected list on its own.
  create_extension_bundle: ({ name, repositoryIds }) =>
    callDaemon('create_extension_bundle', { name, repositoryIds }),
  enqueue_bundle_export: ({ id, destination, mode }) =>
    callDaemon('export_bundle', { bundleId: id, destination: absolute(destination), mode }),
  enqueue_bundle_import: ({ request }) =>
    callDaemon('import_bundle', { archive: absolute(request.archive), conflict: request.conflict }),
  enqueue_container_bundle_install: ({ request }) =>
    callDaemon('enqueue_container_bundle_install', {
      id: request.id,
      profile: request.profile,
      bundleId: request.bundleId,
      conflict: request.conflict,
    }),
  enqueue_container_extension_add: ({ request }) =>
    callDaemon('enqueue_container_extension_add', {
      id: request.id,
      profile: request.profile,
      source: absolute(request.source),
    }),
  enqueue_container_extension_copy: ({ request }) =>
    callDaemon('enqueue_container_extension_copy', {
      id: request.id,
      profile: request.profile,
      repositoryId: request.repositoryId,
    }),
  enqueue_workspace_extension_import: ({ request }) =>
    callDaemon('enqueue_workspace_extension_import', { id: request.id, relativePath: request.relativePath }),
  enqueue_repository_extension_import: ({ request }) =>
    callDaemon('import_repository_extension', { source: absolute(request.source) }),
  enqueue_repository_extension_export: ({ request }) =>
    callDaemon('export_repository_extension', {
      repositoryId: request.repositoryId,
      destination: absolute(request.destination),
    }),
  enqueue_plugin_export: ({ request }) =>
    callDaemon('enqueue_plugin_export', {
      sourceContainerId: request.sourceContainerId,
      sourcePath: absolute(request.sourcePath),
      destination: absolute(request.destination),
    }),
  enqueue_image_build: ({ request }) =>
    callDaemon('enqueue_build', {
      scriptPath: absolute(request.scriptPath),
      outputPath: request.outputPath === null || request.outputPath === undefined
        ? null
        : absolute(request.outputPath),
      containerName: request.containerName,
    }),
  enqueue_pull_template: ({ version }) => callDaemon('pull_template', { ref: `${HARNESS_REPO}:${version}` }),
  remove_repository_plugin: ({ id, profile, name }) => callDaemon('remove_repository_plugin', { id, profile, name }),

  // Both of these answer with the name or path the daemon actually used, while
  // the UI only wants the string.
  export_template: async ({ request }) => {
    const params = { name: request.name }
    if (request.destination) params.destination = absolute(request.destination)
    const value = await callDaemon('export_template', params)
    return value?.path ?? null
  },
  import_template: async ({ request }) => {
    const params = { archive: absolute(request.archive) }
    if (request.name) params.name = request.name
    const value = await callDaemon('import_template', params)
    return value?.name ?? null
  },
  uninstall_dsh_version: async ({ version }) => {
    await callDaemon('uninstall_dsh_version', { version })
    return readConfig()
  },

  // ── opening a container's front end ───────────────────────────────────────
  // The desktop opens a webview window for a container (`open_dsh_front`) or
  // hands its URL to the system browser (`open_dsh_front_browser`). A page can do
  // neither, so both answer with the URL the daemon just minted and the frontend
  // opens a tab — see `openContainerFront` in `src/shared/api/box-api.ts`. The URL
  // carries a per-launch token, which is why it has to be asked for rather than
  // built from the port.
  open_dsh_front: async ({ id }) => (await callDaemon('container_url', { id }))?.url ?? null,
  open_dsh_front_browser: async ({ id }) => (await callDaemon('container_url', { id }))?.url ?? null,

  // ── logs ──────────────────────────────────────────────────────────────────
  // The daemon keeps task logs at the path it reports and container logs under
  // the container directory, but exposes neither as an RPC — the desktop layer
  // reads the files itself. Being able to see a log is most of the reason to
  // debug in a browser at all, so the bridge reads them too.
  read_task_log: async ({ id }) => {
    const tasks = await callDaemon('list_tasks', {})
    const task = (tasks ?? []).find((entry) => entry.id === id)
    if (task === undefined) throw new Error(`task not found: ${id}`)
    return readFileSync(task.logPath, 'utf8')
  },
  read_container_log: async ({ id, log }) => {
    const filename = { host: 'host.log', rebuild: 'rebuild.log', webview: 'webview.log' }[log]
    if (filename === undefined) throw new Error(`unsupported container log: ${log}`)
    const root = readConfig().runtimeDirectory
    if (!root) throw new Error('DSH Box storage is not configured')
    const path = join(root, 'instances', id, 'logs', filename)
    if (!existsSync(path)) return `No ${log} log has been created for this container yet.`
    return readFileSync(path, 'utf8')
  },
}

export function dshboxDevRpcBridge() {
  return {
    name: 'dshbox-dev-rpc-bridge',
    // Serve only: this must never exist in a production build.
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/__rpc', (request, response, next) => {
        if (request.method !== 'POST') return next()
        let body = ''
        request.on('data', (chunk) => { body += chunk })
        request.on('end', async () => {
          const send = (payload) => {
            response.setHeader('Content-Type', 'application/json')
            response.end(JSON.stringify(payload))
          }
          let frame
          try {
            frame = JSON.parse(body || '{}')
          } catch {
            return send({ ok: false, error: 'malformed bridge request' })
          }
          const handler = COMMANDS[frame.command]
          if (handler === undefined) {
            const error = NOT_WIRED(frame.command)
            return send({ ok: false, error: error.message })
          }
          try {
            send({ ok: true, result: await handler(frame.payload ?? {}) })
          } catch (error) {
            send({ ok: false, error: error.message })
          }
        })
      })
      server.config.logger.info(
        `  dshbox dev bridge: /__rpc -> daemon (config ${existsSync(CONFIG_PATH) ? CONFIG_PATH : 'missing'})`,
      )
    },
  }
}
