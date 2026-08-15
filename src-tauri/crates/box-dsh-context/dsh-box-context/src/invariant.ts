/**
 * Runtime-invariant companion for @deepseek-ai/dsh-box-context.
 *
 * The upstream convention (packages/AGENTS.md "Every package owns
 * ./invariant") asks every package to ship a runtime-invariant companion
 * that registers any session log events, registries, or services whose
 * entries could outlive a single apply call. This plugin does not own
 * any of those yet; its iteration-1 body only logs a heartbeat, and
 * iteration 6 will register a single ctx.systemPrompt.context entry
 * scoped to the apply lifetime.
 *
 * The file exists as a documented placeholder; future changes that
 * introduce persistent registrations must replace the body with the
 * matching register call.
 *
 * @module @deepseek-ai/dsh-box-context/invariant
 */

export const name = 'dsh-box-context/invariant'

/** No-op: the apply call owns every registration the plugin contributes. */
export function register(): void {
  /* intentionally empty */
}

export default { name, register }
