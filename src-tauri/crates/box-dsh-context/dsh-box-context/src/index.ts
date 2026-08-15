/**
 * @deepseek-ai/dsh-box-context host half.
 *
 * Iteration 1 (heartbeat): prove DSH actually loads the plugin when
 * NODE_PATH points at the Box vendor directory. The apply function only
 * logs its presence and config; commit 6 will swap the body for the
 * structured ctx.systemPrompt.context registration that mirrors
 * sandbox-policy (110) and user-approval (115).
 *
 * @module @deepseek-ai/dsh-box-context
 */

import type { Context } from '@deepseek-ai/cordis'
import z from '@deepseek-ai/schemastery'

/** Cordis plugin name used by loader diagnostics and the patch overlay. */
export const name = 'dsh-box-context'

/** Services this plugin depends on. systemPrompt is the only registry we
 *  contribute to; declaring it here keeps the loader strict about wiring
 *  order. */
export const inject = ['systemPrompt']

/** Plugin config: location of the JSON snapshot and prompt-section order. */
export interface Config {
  /** Absolute path to the snapshot file Box writes on every container start. */
  contextFile: string
  /** Order of the registered dsh-box:container context section. */
  order?: number
}

/** Schemastery-validated config. The defaults mirror the constants in the
 *  Rust crate (crates/box-dsh-context/src/lib.rs). */
export const Config: z<Config> = z.object({
  contextFile: z.string(),
  order: z.number().default(130),
})

/**
 * Register the plugin against a freshly mounted Cordis context.
 *
 * Iteration 1: log the effective config so host.log proves the plugin is
 * loaded and the patch overlay values reach the apply call. A later
 * commit will replace this body with the actual
 * ctx.systemPrompt.context registration and the JSON file watcher.
 */
export function apply(ctx: Context, config: Config): void {
  ctx.effect(() => {
    // eslint-disable-next-line no-console
    console.log('[dsh-box-context] loaded', JSON.stringify(config))
  }, 'dsh-box-context.heartbeat()')
}

export default { name, inject, Config, apply }
