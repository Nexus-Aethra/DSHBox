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
import { applyBoxTools } from './tools'

/** Cordis plugin name used by loader diagnostics and the patch overlay. */
export const name = 'dsh-box-sandbox'

/**
 * Services this plugin depends on. Empty for now: the client is pure
 * transport and needs no host service. Tools built on top of it will
 * declare their own.
 */
export const inject: string[] = ['tools', 'attachments']

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

/** Register the plugin against a freshly mounted Cordis context. */
export function apply(ctx: Context, config?: Partial<Config>): void {
  resetRpc()
  getRpc(resolveConfig(config))
  applyBoxTools(ctx)
  // Re-apply means a new config; the old singleton must not outlive it.
  ctx.effect(() => () => resetRpc(), 'dsh-box-sandbox.client()')
}
