/**
 * @nexus-aethra/dsh-box-sandbox host half.
 *
 * Makes the DSH Box daemon reachable from inside a DSH session on the same
 * host, so the agent can drive boxes, templates, plugins and resources
 * through a typed transport instead of shelling out to the dshbox CLI and
 * parsing its stdout.
 *
 * The plugin exists because developing a DSH plugin *inside* DSH is a crash
 * story: a bad plugin edit takes down the host that is running it. DSH Box
 * stays external to that loop, so the development sandbox is the one process
 * the agent is allowed to break.
 *
 * Mounting never fails, even when the daemon is down. A missing discovery
 * record is a normal first-run state, not a load error -- it surfaces on the
 * first call, as a message that names the path consulted.
 *
 * @module @nexus-aethra/dsh-box-sandbox
 */

import type { Context } from '@deepseek-ai/cordis'

import {
  DshboxRpc,
  DshboxUnavailableError,
  discoveryPath,
  type Discovery,
} from './rpc'
import { getRpc, resetRpc } from './rpc'
import { applyBoxTools, hostServices } from './tools'

/** Cordis plugin name used by loader diagnostics and the patch overlay. */
export const name = 'dsh-box-sandbox'

/**
 * Services this plugin depends on. Empty for now: the client is pure
 * transport and needs no host service. Tools built on top of it will
 * declare their own.
 */
export const inject: string[] = ['tools', 'attachments', 'systemPrompt']

/**
 * Plugin config. Both fields carry defaults, so the mount works with an
 * empty config block; an empty configDir means "resolve it the way the
 * daemon does" rather than "the current directory".
 */
export interface Config {
  /**
   * Directory holding server/discovery.json. The default empty string
   * means: honour DSHBOX_CONFIG_DIR, then fall back to ~/.dsh-box.
   */
  configDir: string
  /** Per-attempt request timeout in milliseconds. */
  timeoutMs: number
}

/**
 * Effective configuration after defaults.
 *
 * Deliberately not a schemastery schema. A schema would make this entry
 * import `@deepseek-ai/schemastery` at module load, and a bare import that
 * cannot be resolved from wherever the plugin was installed is a *load*
 * failure -- DSH reports it as "failed to import" and the tool never mounts.
 * The two knobs are read from the environment instead, which costs a schema
 * nobody has to keep in sync and makes the bundle import nothing but node:*.
 */
function resolveConfig(config?: Partial<Config>): Config {
  const fromEnv = process.env.DSHBOX_RPC_TIMEOUT_MS
  const parsed = fromEnv === undefined ? Number.NaN : Number(fromEnv)
  return {
    configDir: config?.configDir ?? process.env.DSHBOX_CONFIG_DIR ?? '',
    timeoutMs: config?.timeoutMs ?? (Number.isFinite(parsed) ? parsed : 30_000),
  }
}

export { DshboxRpc, DshboxUnavailableError, discoveryPath, getRpc, resetRpc }
export type { Discovery }

/** Snapshot of reachability, for diagnostics and prompt surfaces. */
export interface Status {
  /** Path the discovery record was looked for at. */
  discoveryPath: string
  /** Endpoint details when the daemon is up, else null. */
  endpoint: Discovery | null
  /** Populated when endpoint is null. */
  problem: string | null
}

/**
 * Probe the daemon without throwing.
 *
 * Returns the discovery record when the daemon answers, or the reason it
 * could not be reached. A ping failure is reported rather than raised so
 * that a status surface can render "dshbox is not running" without every
 * caller having to catch.
 */
export async function status(config?: Partial<Config>): Promise<Status> {
  const rpc = getRpc(resolveConfig(config))
  const path = discoveryPath(rpc.configDir)
  try {
    const record = rpc.discovery()
    const probe = await rpc.ping()
    return { discoveryPath: path, endpoint: record, problem: null }
  } catch (error) {
    const problem =
      error instanceof DshboxUnavailableError
        ? error.message
        : 'dshbox daemon answered but failed its liveness probe'
    // Keep the last known reason alongside the probe detail: "connection
    // refused" and "token rejected" need different user actions.
    return {
      discoveryPath: path,
      endpoint: null,
      problem: error instanceof Error ? error.message + ' (' + problem + ')' : problem,
    }
  }
}

/**
 * Cross-tool guidance, contributed once as a prompt section.
 *
 * Each tool's description already says what that one tool does. What no single
 * description can carry is the order, and getting the order wrong is what costs an
 * agent the most: reading a page, acting on a control that was never on screen, and
 * treating a blocked click as a broken app.
 *
 * Registered inside a cordis effect so the section is removed when this mount is
 * disposed. A duplicate name throws in the host, which would turn a re-apply into a
 * load failure; scoping the registration to the effect is what makes that impossible.
 *
 * Deliberately short. It earns its place by carrying what no tool response repeats;
 * anything a tool already reports belongs in that tool.
 */

const WORKFLOW_PROMPT_SECTION = 'dsh-box-sandbox:page-workflow'

const WORKFLOW_PROMPT_ORDER = 3050
function pageWorkflowPrompt(): string {
  return [
    'Debugging a page with the box_* tools:',
    'Start with box_page_text. It needs no selector and returns every control with its role,',
    'name and position, so it replaces guessing at selectors.',
    'Check the in-viewport flag before acting. A control marked below the fold exists; scroll to',
    'it with box_scroll rather than concluding it is missing.',
    'Act on what box_page_text returned: pass the role and name back to box_click_element rather',
    + 'than a selector or a coordinate, and add within when the same name appears both in a',
    + 'dialog and on the page behind it. A click reports whether it',
    'landed, so believe that field, not the fact that you dispatched a click: when landed is',
    'false something covered the target, and a page that looks unchanged is not a broken app.',
    'Before typing, click the field. box_type_text reports the focused element and inserts nothing',
    'when no editable field holds focus.',
    'If a page tool cannot find a browser, box_set_browser reports which one is in use and pins a',
    'different one.',
  ].join('\n')
}
/** Register the plugin against a freshly mounted Cordis context. */
export function apply(ctx: Context, config?: Partial<Config>): void {
  resetRpc()
  getRpc(resolveConfig(config))
  applyBoxTools(ctx)
  // Re-apply means a new config; the old singleton must not outlive it.
  ctx.effect(() => () => resetRpc(), 'dsh-box-sandbox.client()')
  // The section is registered inside an effect, so disposing this mount
  // removes it. Registering it unconditionally would make the second mount
  // throw on a duplicate section name and take every tool down with it.
  ctx.effect(() => hostServices(ctx).systemPrompt.section({
    name: WORKFLOW_PROMPT_SECTION,
    order: WORKFLOW_PROMPT_ORDER,
    text: pageWorkflowPrompt,
  }), 'dsh-box-sandbox.systemPrompt()')
}
