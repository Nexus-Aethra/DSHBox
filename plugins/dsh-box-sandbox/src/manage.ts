/**
 * Agent-facing tools that drive dshbox itself: containers, templates,
 * boxfiles and resources.
 *
 * These are grouped by what a caller is trying to do, not one tool per RPC.
 * The daemon answers 89 methods, and a tool per method would hand the model
 * 89 ways to ask the same handful of questions -- the same burden that made
 * `box_query_elements` worth deleting. So `box_lifecycle` is start, stop,
 * restart, rebuild and remove; `box_resources` is list, read, write, extract
 * and inject. One tool per intent, with the choice inside it.
 *
 * They call the daemon directly rather than shelling out to the dshbox CLI.
 * Same daemon, same single implementation of every rule -- but no process to
 * spawn, no output to parse, and a failure that arrives as a message instead
 * of an exit code plus stderr. The CLI stays for humans and for scripting.
 *
 * Anything that is a background task is enqueued *and waited on* here, so a
 * call returns an outcome rather than a task id. See tasks.ts for why the
 * distinction between finished and terminal is not the one it looks like.
 *
 * @module dsh-box-sandbox/manage
 */

import type { Context } from '@deepseek-ai/cordis'

import { dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

import { getRpc } from './rpc'
import { runTask, type TaskRecord } from './tasks'
import { hostServices, type HostToolDefinition } from './tools'

/** A long operation; a container start can sit on a prepare for minutes. */
const LONG_BUDGET_MS = 900_000

/** Read a value that must be a non-empty string, or fail naming the field. */
function required(args: Record<string, unknown>, field: string): string {
  const value = args[field]
  if (typeof value !== 'string' || value.trim() === '') {
    throw new Error(`${field} is required and must be a non-empty string.`)
  }
  return value.trim()
}

/** Read an optional string, treating blank as absent. */
function optional(args: Record<string, unknown>, field: string): string | undefined {
  const value = args[field]
  if (typeof value !== 'string') return undefined
  const trimmed = value.trim()
  return trimmed === '' ? undefined : trimmed
}

/** Read an optional boolean that only an explicit `true` turns on. */
function flag(args: Record<string, unknown>, field: string): boolean {
  return args[field] === true
}

/** Reject an action a tool does not implement, listing the ones it does. */
function badAction(tool: string, given: unknown, allowed: readonly string[]): never {
  throw new Error(
    `${String(given)} is not a ${tool} action. Use one of: ${allowed.join(', ')}.`,
  )
}

/** Shared shape for the JSON Schema of a single-property action. */
function actionProperty(tool: string, allowed: readonly string[]): Record<string, unknown> {
  return {
    type: 'string',
    enum: [...allowed],
    description: `What to do. One of: ${allowed.join(', ')} (${tool}).`,
  }
}

/** A text-only result with a schema that names what the caller can read. */
function textOutput(
  properties: Record<string, unknown>,
  required: string[],
  render: (value: Record<string, unknown>) => string,
): Pick<HostToolDefinition['output'], 'schema' | 'render'> {
  return {
    schema: { type: 'object', additionalProperties: false, properties, required },
    render: (_args: unknown, value: Record<string, unknown>) => [
      { type: 'text', text: render(value) },
    ],
  }
}
/** Register one tool against the host's tools service, the same path the
 *  page-debugging tools take, so both halves live in one namespace.
 */
function register(ctx: Context, definition: HostToolDefinition): void {
  hostServices(ctx).tools.register(definition)
}

/** Register the dshbox management tools. */
export function registerManageTools(ctx: Context): void {
  register(ctx, {
    name: 'box_lifecycle',
    description:
      'Start, stop, restart, rebuild or remove a dshbox container, and wait for it to finish. '
      + 'This is the tool for anything that changes whether a container runs. It blocks until the '
      + 'operation completes and returns the outcome, so a restart that failed comes back as a '
      + 'failure with the reason rather than a task id and silence. Use it instead of running '
      + 'dshbox on a shell: a start that quietly did not start is the failure this removes. '
      + 'Rebuild re-materialises plugins and resources before starting; restart does not. '
      + 'After a start, box_overview reports the new state. Every action is refused when'
      + ' the target is the container this agent is itself running in, because it restarts'
      + ' or stops the host running the call and the result can never come back. Watch'
      + ' progress with box_task and confirm with box_overview instead.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty('lifecycle', ['start', 'stop', 'restart', 'rebuild', 'remove']),
        containerId: {
          type: 'string',
          description: 'Container to act on. box_overview lists the ids.',
        },
      },
      required: ['action', 'containerId'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      { action: { type: 'string' }, containerId: { type: 'string' }, kind: { type: 'string' } },
      ['action', 'containerId', 'kind'],
      (value) =>
        `Container ${String(value.containerId)}: ${String(value.action)} finished `
        + `as ${String(value.kind)}.`,
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const containerId = required(record, 'containerId')
      const action = required(record, 'action')
      const methods: Record<string, string> = {
        start: 'enqueue_container_start',
        stop: 'enqueue_container_stop',
        restart: 'enqueue_container_restart',
        rebuild: 'enqueue_container_rebuild',
        remove: 'delete_container',
      }
      const method = methods[action]
      if (method === undefined) {
        badAction('lifecycle', action, Object.keys(methods))
      }
      // Every action here restarts or stops the container this call is running
      // inside, which ends the host executing it. Restart looked safe -- the
      // session does come back -- but the in-flight call is not: the model asked
      // for something, the host died before answering, and on reconnect the
      // model waits on a result that can never arrive. Measured, not assumed: a
      // restart issued from inside showed the tool call in the transcript with
      // no answer and the client left reconnecting. So the whole tool refuses
      // self-targeting, and says what to do instead.
      if (runsInside(containerId)) {
        throw new Error(
          `This agent is running inside ${containerId}, and ${action} would restart or stop`
            + ' the host running this call, so the result could never be reported -- the'
            + ' session reconnects and the call is simply lost. Cycle the container from'
            + ' outside it (the Box UI, or a dshbox command in a separate terminal), then'
            + ' call box_task to watch the work, or box_overview to confirm the state.'
        )
      }
      const task: TaskRecord = await runTask(method, { id: containerId })
      return { action, containerId, kind: task.kind }
    },
  })

  register(ctx, {
    name: 'box_overview',
    description:
      'What dshbox currently holds: its containers, its templates and the plugins it knows. '
      + 'Call this first when you do not know what exists, and after any change that could '
      + 'have added or removed something. It answers from the store, so the container ids it '
      + 'prints are the ones box_lifecycle and box_resources accept.',
    parameters: {
      type: 'object',
      properties: {
        include: {
          type: 'string',
          description:
            'Comma-separated: box, containers, templates, plugins. Defaults to all four.',
        },
      },
      required: [],
    },
    timeoutMs: 60_000,
    output: textOutput(
      {
        box: { type: 'object' },
        containers: { type: 'array', items: { type: 'object' } },
        templates: { type: 'array', items: { type: 'object' } },
        plugins: { type: 'array', items: { type: 'object' } },
      },
      ['box', 'containers', 'templates', 'plugins'],
      (value) => {
        const lines: string[] = []
        const box = (value.box ?? {}) as Record<string, unknown>
        const containers = (value.containers ?? []) as Record<string, unknown>[]
        const templates = (value.templates ?? []) as Record<string, unknown>[]
        const plugins = (value.plugins ?? []) as Record<string, unknown>[]
        const stamp = box.buildStamp !== undefined ? ` (build ${String(box.buildStamp)})` : ''
        lines.push(
          `Box${stamp}: `
          + Object.entries(box)
            .filter(([key]) => key !== 'buildStamp')
            .map(([key, entry]) => `${key}=${typeof entry === 'object' ? JSON.stringify(entry) : String(entry)}`)
            .join('  '),
        )
        lines.push(
          `Containers (${containers.length}):`,
          ...containers.map(
            (row) =>
              `  ${String(row.id)}  ${String(row.name ?? '')}  ${String(row.status ?? '')}`,
          ),
        )
        lines.push(
          `Templates (${templates.length}):`,
          ...templates.map((row) => `  ${String(row.name)}  ${String(row.kind ?? '')}`),
        )
        lines.push(
          `Plugins (${plugins.length}):`,
          ...plugins.map((row) => `  ${String(row.name ?? row.id)}`),
        )
        return lines.join('\n')
      },
    ),
    async execute(args) {
      const want = (optional(args as Record<string, unknown>, 'include') ?? 'box,containers,templates,plugins')
        .split(',')
        .map((part) => part.trim())
        .filter((part) => part !== '')
      const rpc = getRpc()
      const result: Record<string, unknown> = { box: {}, containers: [], templates: [], plugins: [] }
      if (want.includes('box')) {
        // What the box itself reports: counts, the runtime it is using, the
        // build it is on. Useful before acting, because it is the one answer
        // that says whether this is a fresh install or something already set up.
        result.box = asObject(await rpc.call('get_info'))
      }
      if (want.includes('containers')) {
        result.containers = asArray(await rpc.call('list_containers'))
      }
      if (want.includes('templates')) {
        result.templates = asArray(await rpc.call('list_templates'))
      }
      if (want.includes('plugins')) {
        result.plugins = asArray(await rpc.call('list_repository_extensions'))
      }
      return result
    },
  })
  register(ctx, {
    name: 'box_resources',
    description:
      'The state a container persists, and the tools to move it: list what is there, read one '
      + 'file as a YAML tree, write a block, or extract and inject it as a reusable resource. '
      + 'Everything that changes a container waits for the operation to finish and reports the '
      + 'result, so an injection that was refused arrives as a refusal. Reading is the cheapest '
      + 'way to see what a container actually has, including provider credentials -- a secret\'s '
      + 'existence and size, never its contents.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty(
          'resources',
          ['list', 'read', 'write', 'extract', 'inject', 'rm', 'rm-view', 'prune'],
        ),
        containerId: { type: 'string', description: 'Container to act on.' },
        path: {
          type: 'string',
          description:
            'Container-relative file. Required for read and write, and names the resource '
            + 'kind\'s default location for extract.',
        },
        kind: { type: 'string', description: 'Resource kind, e.g. sessions or credentials.' },
        section: {
          type: 'string',
          description: 'YAML key path to read or write at, e.g. llm-pi-ai. Empty means the whole file.',
        },
        text: { type: 'string', description: 'The YAML block to place at --section.' },
        resourceId: { type: 'string', description: 'Which stored resource to inject or remove.' },
        viewId: { type: 'string', description: 'Which resource view to remove.' },
        name: { type: 'string', description: 'Name for an extracted resource.' },
        merge: { type: 'boolean', description: 'Merge into an existing destination instead of refusing.' },
        overwrite: { type: 'boolean', description: 'Replace the destination first.' },
        restart: {
          type: 'boolean',
          description: 'Stop a running container, act, then start it again.',
        },
      },
      required: ['action', 'containerId'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      {
        action: { type: 'string' },
        containerId: { type: 'string' },
        result: { type: 'object' },
      },
      ['action', 'containerId', 'result'],
      (value) => {
        const payload = value.result
        return `Container ${String(value.containerId)}: ${String(value.action)}.\n`
          + JSON.stringify(payload, null, 2).slice(0, 6000)
      },
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const containerId = required(record, 'containerId')
      const action = required(record, 'action')
      const rpc = getRpc()
      const base = { id: containerId }
      const carry = (extra: Record<string, unknown>) => {
        const params: Record<string, unknown> = { ...base, ...extra }
        if (flag(record, 'merge')) params.merge = true
        if (flag(record, 'overwrite')) params.overwrite = true
        if (flag(record, 'restart')) params.restart = true
        return params
      }
      switch (action) {
        case 'list': {
          const request: Record<string, unknown> = { ...base }
          const kind = optional(record, 'kind')
          if (kind !== undefined) request.plugin = kind
          return { action, containerId, result: await rpc.call('list_container_resources', request) }
        }
        case 'read':
          return {
            action,
            containerId,
            result: await rpc.call('read_resource_tree',
              carry({ path: required(record, 'path'), section: optional(record, 'section') ?? '' }),
            ),
          }
        case 'write': {
          const params = carry({
            path: required(record, 'path'),
            section: optional(record, 'section') ?? '',
            text: required(record, 'text'),
          })
          const task = await runTask('enqueue_resource_write', params)
          return { action, containerId, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'extract': {
          const params = carry({
            kind: required(record, 'kind'),
            name: optional(record, 'name'),
            path: optional(record, 'path'),
          })
          const task = await runTask('enqueue_resource_extract', params)
          return { action, containerId, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'inject': {
          const params = carry({ resourceId: required(record, 'resourceId') })
          const task = await runTask('enqueue_resource_inject', params)
          return { action, containerId, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'rm':
          // Removal is the one action here with no undo, so it takes a resource
          // id and nothing else: it cannot be inferred, and a wrong guess would
          // delete state rather than fail.
          return {
            action,
            containerId,
            result: asObject(
              await rpc.call('delete_resource', { resourceId: required(record, 'resourceId') }),
            ),
          }
        case 'rm-view':
          return {
            action,
            containerId,
            result: asObject(
              await rpc.call('delete_resource_view', { id: required(record, 'viewId') }),
            ),
          }
        case 'prune':
          return { action, containerId, result: { removed: await rpc.call('prune_orphaned_data') } }
        default:
          badAction('resources', action, ['list', 'read', 'write', 'extract', 'inject', 'rm', 'rm-view', 'prune'])
      }
    },
  })
  register(ctx, {
    name: 'box_build',
    description:
      'Build a template from a boxfile, and wait for the build to finish. '
      + 'A template is what you then create a container from. The path is read from the host, so '
      + 'give an absolute one. Applying a resource document is deliberately not here: the daemon '
      + 'has no single method for it, it is a sequence the CLI drives, and re-implementing that '
      + 'sequence in a tool is how two copies drift apart. Use box_resources for moving state.',
    parameters: {
      type: 'object',
      properties: {
        mode: {
          type: 'string',
          enum: ['build', 'apply'],
          description:
            '`build` compiles a boxfile into a template. `apply` makes a resource layer match a document.',
        },
        file: {
          type: 'string',
          description: 'Absolute path to the boxfile, or to the apply document.',
        },
        document: {
          type: 'string',
          description: 'An apply document as YAML or JSON, given inline instead of a file.',
        },
        containerId: {
          type: 'string',
          description: 'Which container to apply to. Not used by build.',
        },
        dryRun: {
          type: 'boolean',
          description: 'Report what apply would change without changing it.',
        },
      },
      required: ['mode'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      { mode: { type: 'string' }, file: { type: 'string' }, result: { type: 'object' } },
      ['mode', 'file', 'result'],
      (value) =>
        `${String(value.mode)} ${String(value.file)}:\n`
        + JSON.stringify(value.result, null, 2).slice(0, 4000),
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const mode = required(record, 'mode')
      const file = optional(record, 'file') ?? ''
      if (mode === 'build') {
        const task = await runTask('enqueue_build', { path: file, name: optional(record, 'containerId') })
        return { mode, file, result: { kind: task.kind, logPath: task.logPath ?? '' } }
      }
      if (mode === 'apply') {
        const rpc = getRpc()
        const params: Record<string, unknown> = { dryRun: flag(record, 'dryRun') }
        const document = optional(record, 'document')
        if (document !== undefined) params.document = document
        if (file !== '') params.file = file
        const applied = await rpc.call<Record<string, unknown>>('apply_document', params)
        return { mode, file, result: applied }
      }
      badAction('build', mode, ['build', 'apply'])
    },
  })

  register(ctx, {
    name: 'box_workspace',
    description:
      'The directories a container opens its sessions in. add registers one from a path with no '
      + 'file dialog, which is the point: the picker the UI uses opens on the host desktop and '
      + 'nothing driving the app from a script or a tool call can answer it. Add refuses while '
      + 'the container runs, because the running host rewrites its own registry; pass restart to '
      + 'stop, register and start again in that order. Adding a path that is already registered '
      + 'reports it as such rather than failing.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty('workspace', ['list', 'add']),
        containerId: { type: 'string', description: 'Container whose workspaces to list or extend.' },
        path: { type: 'string', description: 'Absolute path of an existing directory. Required for add.' },
        title: { type: 'string', description: 'Name for it. Defaults to the directory\'s own name.' },
        restart: { type: 'boolean', description: 'Stop the container, add, then start it again.' },
      },
      required: ['action', 'containerId'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      {
        action: { type: 'string' },
        containerId: { type: 'string' },
        workspaces: { type: 'array', items: { type: 'object' } },
        created: { type: 'boolean' },
      },
      ['action', 'containerId', 'workspaces'],
      (value) => {
        const rows = (value.workspaces ?? []) as Record<string, unknown>[]
        const head = `Workspaces of ${String(value.containerId)} (${rows.length}):`
        return [
          head,
          ...rows.map((row) => `  ${String(row.title)}  ${String(row.path)}  ${String(row.sessions ?? 0)} sessions`),
        ].join('\n')
      },
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const containerId = required(record, 'containerId')
      const action = required(record, 'action')
      const rpc = getRpc()
      if (action === 'list') {
        const listed = await rpc.call<{ workspaces?: unknown[] }>('list_container_workspaces', { id: containerId })
        return {
          action,
          containerId,
          workspaces: (listed?.workspaces ?? []) as unknown[],
          created: false,
        }
      }
      if (action === 'add') {
        const params: Record<string, unknown> = {
          id: containerId,
          path: required(record, 'path'),
        }
        const title = optional(record, 'title')
        if (title !== undefined) params.title = title
        const added = await rpc.call<{ created?: boolean }>('add_container_workspace', params)
        const listed = await rpc.call<{ workspaces?: unknown[] }>('list_container_workspaces', { id: containerId })
        return {
          action,
          containerId,
          workspaces: listed.workspaces ?? [],
          created: added?.created === true,
        }
      }
      badAction('workspace', action, ['list', 'add'])
    },
  })
  register(ctx, {
    name: 'box_set_viewport',
    description:
      'Resize the page an agent is looking at, and report the size now in effect. '
      + 'The page opens at 1600x1200, which fits most applications on one screen, but a wide '
      + 'table or a tall form may still fold. A fold is not cosmetic: box_page_text marks '
      + 'anything below the fold, so a control that is in fact visible reads as something to '
      + 'scroll for, and box_click_element on it is a wasted round trip. Make it taller, or '
      + 'wider when a row is being cut off on the right, then read the page again. Resizing '
      + 'keeps the page where it is, so a session survives the change.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Which debug session to resize.' },
        width: {
          type: 'number',
          description: 'Width in CSS pixels. 320-7680. Omit to read the current size.',
        },
        height: {
          type: 'number',
          description: 'Height in CSS pixels. 240-4320. Omit to read the current size.',
        },
      },
      required: ['containerId'],
    },
    timeoutMs: 60_000,
    output: textOutput(
      {
        width: { type: 'number' },
        height: { type: 'number' },
        clamped: { type: 'boolean' },
      },
      ['width', 'height'],
      (value) =>
        `Viewport is now ${String(value.width)}x${String(value.height)}`
        + (value.clamped === true
          ? '. That is smaller than it was asked for: the browser has a maximum.'
          : '.'),
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const containerId = required(record, 'containerId')
      const rpc = getRpc()
      const params: Record<string, unknown> = { id: containerId }
      const width = number(record, 'width')
      const height = number(record, 'height')
      if (width === undefined && height === undefined) {
        // With neither, this is a question rather than an instruction, and the
        // layout metrics are what actually answer it -- the window size that
        // was requested is not the viewport the page is laid out into.
        const metrics = await rpc.call<Record<string, unknown>>('debug_set_viewport', params)
        return { width: Number(metrics.width), height: Number(metrics.height), clamped: false }
      }
      if (width === undefined || height === undefined) {
        throw new Error('Give both width and height to resize the viewport.')
      }
      const result = await rpc.call<Record<string, unknown>>('debug_set_viewport', {
        ...params,
        width,
        height,
      })
      return {
        width: Number(result.width),
        height: Number(result.height),
        clamped: result.clamped === true,
      }
    },
  })
  register(ctx, {
    name: 'box_task',
    description:
      'Watch, read or cancel dshbox background work. Most box_* calls already wait for their own '
      + 'operation, so reach for this when something is already running, when you want to see what '
      + 'a previous call left behind, or to stop work that should not continue. A task log is a '
      + 'file, and its path comes back with the record.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty('task', ['list', 'status', 'cancel', 'delete']),
        taskId: { type: 'string', description: 'Which task. Required for status, cancel and delete.' },
        kind: {
          type: 'string',
          description: 'Only list tasks of this kind, e.g. container-start. Omit for all.',
        },
      },
      required: ['action'],
    },
    timeoutMs: 60_000,
    output: textOutput(
      { action: { type: 'string' }, tasks: { type: 'array', items: { type: 'object' } } },
      ['action', 'tasks'],
      (value) => {
        const rows = (value.tasks ?? []) as Record<string, unknown>[]
        return [
          `Tasks (${rows.length}):`,
          ...rows.map(
            (row) =>
              `  ${String(row.id)}  ${String(row.status)}  ${String(row.progress)}%  ${String(row.kind)}`
              + (row.error ? `  error: ${String(row.error)}` : ''),
          ),
        ].join('\n')
      },
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const action = required(record, 'action')
      const rpc = getRpc()
      const one = async () => {
        const task = await rpc.call<TaskRecord>('task_status', { id: required(record, 'taskId') })
        return { action, tasks: [task as unknown] }
      }
      switch (action) {
        case 'list': {
          const all = asArray(await rpc.call('list_tasks'))
          const kind = optional(record, 'kind')
          const filtered = kind === undefined ? all : all.filter((row) => String(row.kind) === kind)
          return { action, tasks: filtered }
        }
        case 'status':
          return one()
        case 'cancel': {
          const task = await rpc.call<TaskRecord>('cancel_task', { id: required(record, 'taskId') })
          return { action, tasks: [task as unknown] }
        }
        case 'delete': {
          await rpc.call('delete_task', { id: required(record, 'taskId') })
          return { action, tasks: [] }
        }
        default:
          badAction('task', action, ['list', 'status', 'cancel', 'delete'])
      }
    },
  })
  register(ctx, {
    name: 'box_plugins',
    description:
      'Plugins a box has, and the ones it can have: the repository, a container, '
      + 'install, import, export, bundles, and the dependency graph. Installing and '
      + 'importing are waited on, so a call ends with a result rather than a task '
      + 'id. One containerId covers all of it: the daemon names that argument '
      + 'differently per method, which is a detail of its wire format and not '
      + 'something a caller should have to remember per verb.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty(
          'plugins',
          ['list', 'installed', 'container', 'add', 'remove', 'import', 'export', 'bundle', 'prune', 'graph'],
        ),
        containerId: { type: 'string', description: 'Container to read, install into, or graph.' },
        profile: { type: 'string', description: 'Profile inside the container. Defaults to web.' },
        spec: { type: 'string', description: 'What to install: a package spec, a directory, or a row id.' },
        extensionId: { type: 'string', description: 'Which repository extension to export.' },
        name: { type: 'string', description: 'Bundle name, for a bundle action.' },
        ids: { type: 'array', items: { type: 'string' }, description: 'Repository row ids that make up a bundle.' },
        subaction: { type: 'string', enum: ['list', 'create', 'import', 'export', 'delete'], description: 'What to do with a bundle.' },
      },
      required: ['action'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      { action: { type: 'string' }, result: { type: 'object' } },
      ['action', 'result'],
      (value) => 'box_plugins ' + String(value.action) + ':\n' + JSON.stringify(value.result, null, 2).slice(0, 6000),
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const action = required(record, 'action')
      const rpc = getRpc()
      const containerId = optional(record, 'containerId')
      const profile = optional(record, 'profile') ?? 'web'
      const needContainer = (what: string): string => {
        if (containerId === undefined) {
          throw new Error(action + ' ' + what + ' needs a containerId. box_overview lists them.')
        }
        return containerId
      }
      switch (action) {
        case 'list':
          return { action, result: { repository: asArray(await rpc.call('list_repository_extensions')) } }
        case 'installed':
          return { action, result: asObject(await rpc.call('list_installed_plugins')) }
        case 'container':
          return { action, result: { plugins: await rpc.call('container_list_plugins', { containerId: needContainer('listing'), profile }) } }
        case 'add': {
          const spec = required(record, 'spec')
          const task = await runTask('container_plugin_add', { id: needContainer('installing into'), profile, spec })
          return { action, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'import': {
          const spec = required(record, 'spec')
          const task = await runTask('import_repository_extension', { spec })
          return { action, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'export':
          return { action, result: asObject(await rpc.call('export_repository_extension', { id: required(record, 'extensionId') })) }
        case 'remove':
          return {
            action,
            result: asObject(
              await rpc.call('remove_repository_extension', { id: required(record, 'extensionId') }),
            ),
          }
        case 'prune':
          return { action, result: { removed: asArray(await rpc.call('prune_repository_extensions')) } }
        case 'bundle': {
          const name = optional(record, 'name')
          const ids = Array.isArray(record.ids) ? (record.ids as string[]).filter((e) => typeof e === 'string') : []
          const sub = optional(record, 'subaction') ?? (name === undefined ? 'list' : ids.length === 0 ? 'create' : 'import')
          if (sub === 'list') return { action, result: { bundles: asArray(await rpc.call('list_bundles')) } }
          if (name === undefined && ids.length === 0) {
            throw new Error('bundle ' + sub + ' needs a name or ids.')
          }
          if (sub === 'create') {
            return { action, result: asObject(await rpc.call('create_extension_bundle', { name, repositoryIds: ids })) }
          }
          if (sub === 'export') {
            return { action, result: asObject(await rpc.call('export_bundle', { name })) }
          }
          if (sub === 'delete') {
            return { action, result: asObject(await rpc.call('delete_extension_bundle', { name })) }
          }
          return { action, result: asObject(await rpc.call('import_bundle', { name, ids })) }
        }
        case 'graph':
          return { action, result: asObject(await rpc.call('plugin_dependency_graph', { id: needContainer('graphing'), kind: 'container' })) }
        default:
          badAction(
            'plugins',
            action,
            ['list', 'installed', 'container', 'add', 'remove', 'import', 'export', 'bundle', 'prune', 'graph'],
          )
      }
    },
  })
  register(ctx, {
    name: 'box_create',
    description:
      'Make a container, and look at one in full. An agent that can start and stop '
      + 'containers but not create them is doing half the job: create from a '
      + 'template, create from a DSH version, describe what exists, get the URL a '
      + 'running one is serving, or list the directories a container can open. '
      + 'Creating from a template starts the container and returns its id, so the '
      + 'next call can act on it. A plain create materialises the tree without '
      + 'starting anything; start it with box_lifecycle. The name has to be safe '
      + 'for a path, and is not the id.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty('create', ['from-template', 'from-version', 'describe', 'url', 'browse']),
        name: { type: 'string', description: 'Name for a new container, or which one to describe.' },
        template: { type: 'string', description: 'Template to create from. Defaults to the one box_templates reports.' },
        version: { type: 'string', description: 'DSH version to create from, for from-version.' },
        containerId: { type: 'string', description: 'Which container, for describe, url and browse.' },
        profile: { type: 'string', description: 'Profile to create with. Defaults to web.' },
        start: { type: 'boolean', description: 'Start it after creating. Only from-version; from-template always starts.' },
      },
      required: ['action'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      { action: { type: 'string' }, result: { type: 'object' } },
      ['action', 'result'],
      (value) => 'box_create ' + String(value.action) + ':\n' + JSON.stringify(value.result, null, 2).slice(0, 6000),
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const action = required(record, 'action')
      const rpc = getRpc()
      const profile = optional(record, 'profile') ?? 'web'
      const needId = (): string => {
        const id = optional(record, 'containerId')
        if (id === undefined) {
          throw new Error('create ' + action + ' needs a containerId. box_overview lists them.')
        }
        return id
      }
      switch (action) {
        case 'from-template': {
          const name = required(record, 'name')
          const template = optional(record, 'template')
          if (template === undefined) {
            throw new Error('from-template needs a template name; box_templates with action list reports what exists.')
          }
          const task = await runTask('create_container_from_template', { name, template, profile })
          return { action, result: { kind: task.kind, name, logPath: task.logPath ?? '' } }
        }
        case 'from-version': {
          const name = required(record, 'name')
          const version = optional(record, 'version')
          if (version === undefined) {
            throw new Error('from-version needs a DSH version; box_templates with action catalog reports what exists.')
          }
          const created = await rpc.call<Record<string, unknown>>('create_container', { name, version, profile })
          const id = created && created.id !== undefined ? String(created.id) : ''
          if (id !== '' && flag(record, 'start')) {
            await runTask('enqueue_container_start', { id })
          }
          return { action, result: { ...created, started: id !== '' && flag(record, 'start') } }
        }
        case 'describe':
          return { action, result: asObject(await rpc.call('describe_container', { id: needId() })) }
        case 'url': {
          const url = await rpc.call<Record<string, unknown>>('container_url', { id: needId() })
          return { action, result: asObject(url) }
        }
        case 'browse':
          return { action, result: asObject(await rpc.call('browse_container_paths', { id: needId() })) }
        default:
          badAction('create', action, ['from-template', 'from-version', 'describe', 'url', 'browse'])
      }
    },
  })

  register(ctx, {
    name: 'box_templates',
    description:
      'The templates a box can create containers from, and the DSH versions behind '
      + 'them: list them, read one, pull a newer one, import an archive, export one '
      + 'out, or remove one. Pulling and importing are waited on. A template is the '
      + 'shape a new container starts from, so this is what you look at before '
      + 'box_create from-template. Removing one is refused while a container was '
      + 'built from it.',
    parameters: {
      type: 'object',
      properties: {
        action: actionProperty(
          'templates',
          ['list', 'show', 'pull', 'import', 'export', 'remove', 'prune', 'catalog'],
        ),
        name: { type: 'string', description: 'Which template, or which DSH version to pull.' },
        archive: { type: 'string', description: 'Absolute path of an archive to import.' },
        destination: { type: 'string', description: 'Where to write an export. Defaults to a temp file.' },
        versions: { type: 'boolean', description: 'With list, also report installed DSH versions.' },
      },
      required: ['action'],
    },
    timeoutMs: LONG_BUDGET_MS,
    output: textOutput(
      { action: { type: 'string' }, result: { type: 'object' } },
      ['action', 'result'],
      (value) => 'box_templates ' + String(value.action) + ':\n' + JSON.stringify(value.result, null, 2).slice(0, 6000),
    ),
    async execute(args) {
      const record = args as Record<string, unknown>
      const action = required(record, 'action')
      const rpc = getRpc()
      switch (action) {
        case 'list': {
          const templates = asArray(await rpc.call('list_templates'))
          const result: Record<string, unknown> = { templates }
          if (flag(record, 'versions')) {
            result.dshVersions = asObject(await rpc.call('list_installed_dsh_versions'))
          }
          return { action, result }
        }
        case 'show':
          return { action, result: asObject(await rpc.call('read_template', { name: required(record, 'name') })) }
        case 'catalog':
          return { action, result: asObject(await rpc.call('list_dsh_catalog')) }
        case 'pull': {
          const task = await runTask('pull_template', { name: required(record, 'name') })
          return { action, result: { kind: task.kind, logPath: task.logPath ?? '' } }
        }
        case 'import': {
          const archive = required(record, 'archive')
          const params: Record<string, unknown> = { archive }
          const name = optional(record, 'name')
          if (name !== undefined) params.name = name
          return { action, result: asObject(await rpc.call('import_template', params)) }
        }
        case 'export': {
          const params: Record<string, unknown> = { name: required(record, 'name') }
          const destination = optional(record, 'destination')
          if (destination !== undefined) params.destination = destination
          return { action, result: asObject(await rpc.call('export_template', params)) }
        }
        case 'remove':
          return { action, result: asObject(await rpc.call('remove_template', { name: required(record, 'name') })) }
        case 'prune':
          return { action, result: { removed: asArray(await rpc.call('prune_template_snapshots')) } }
        default:
          badAction(
            'templates',
            action,
            ['list', 'show', 'pull', 'import', 'export', 'remove', 'prune', 'catalog'],
          )
      }
    },
  })
}



  /** Keep a structured RPC reply an object, so rendering never sees an array. */
  function asObject(value: unknown): Record<string, unknown> {
    if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
      return value as Record<string, unknown>
    }
    return { value }
  }


/**
 * True when this plugin is running inside the container it was asked about.
 *
 * Nothing hands a plugin its own container id -- the host process is told which
 * container it is, but not the plugins it mounts -- so it is read back out of
 * where the plugin is installed, which is always
 * `<instance>/profile/profiles/<p>/node_modules/...`: the container id is a
 * directory name on the way to it.
 *
 * Comparing path segments to the id that was passed in, rather than testing the
 * id's shape, is the point. An earlier version matched a UUID 8-4-4-4-12 and
 * silently never matched anything: real ids are `container-<10 digits>-<uuid>`,
 * with a unix timestamp in front. A guard that quietly does not fire is worse
 * than no guard, because the code reads as though the case is handled.
 */
function runsInside(containerId: string): boolean {
  let here: string
  try {
    here = dirname(fileURLToPath(import.meta.url))
  } catch {
    return false
  }
  return here.split(/[\\/]/).includes(containerId)
}
/** Read an optional finite number, treating blank and NaN as absent. */
function number(args: Record<string, unknown>, field: string): number | undefined {
  const value = args[field]
  if (typeof value !== 'number' || !Number.isFinite(value)) return undefined
  return Math.round(value)
}
/** The daemon answers list-shaped methods with a bare array; guard that here. */
function asArray(value: unknown): Record<string, unknown>[] {
  if (Array.isArray(value)) return value as Record<string, unknown>[]
  if (typeof value === 'object' && value !== null) {
    for (const key of ['containers', 'templates', 'plugins', 'items', 'rows']) {
      const nested = (value as Record<string, unknown>)[key]
      if (Array.isArray(nested)) return nested as Record<string, unknown>[]
    }
  }
  return []
}
