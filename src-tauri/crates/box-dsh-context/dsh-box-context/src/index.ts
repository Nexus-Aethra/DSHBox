/**
 * @deepseek-ai/dsh-box-context host half.
 *
 * Iteration 6: registers a ctx.systemPrompt.context(...) section that
 * surfaces the DSH Box container metadata to the agent as a structured
 * user-role history snapshot. Replaces the iteration-1 heartbeat body.
 *
 * The plugin reads the JSON snapshot file Box writes on every container
 * start, renders it into a structured prompt section, and re-renders on
 * file change so an in-flight agent sees updates without a host restart.
 *
 * @module @deepseek-ai/dsh-box-context
 */

import { readFileSync, watch } from 'node:fs'
import type { Context } from '@deepseek-ai/cordis'
import z from '@deepseek-ai/schemastery'

/** Cordis plugin name used by loader diagnostics and the patch overlay. */
export const name = 'dsh-box-context'

/** Services this plugin depends on. */
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

interface Snapshot {
  container?: { id?: string; name?: string; version?: string; profile?: string }
  paths?: Partial<Record<'workspace' | 'profileHome' | 'plugins' | 'skills' | 'logs' | 'dshboxHome' | 'dshboxCli', string>>
  credentials?: { providers?: Array<{ apiKeyEnv?: string }> }
}

function loadSnapshot(path: string): Snapshot | null {
  try {
    return JSON.parse(readFileSync(path, 'utf8')) as Snapshot
  } catch {
    return null
  }
}

function field(value: string | undefined, fallback: string): string {
  return typeof value === 'string' && value.length > 0 ? value : fallback
}

/** Render the snapshot as a structured prompt section. The agent should
 *  treat the leading prefix and the `key = value` lines as authoritative
 *  facts it can read directly rather than as narrative prose. */
function renderSnapshot(s: Snapshot | null): string {
  if (!s) return ''
  const c = s.container ?? {}
  const p = s.paths ?? {}
  const creds = s.credentials && s.credentials.providers ? s.credentials.providers : []
  const providers: string[] = [];
  for (const item of creds) {
    const env = item && item.apiKeyEnv;
    if (typeof env === 'string' && env.length > 0) providers.push(env);
  }
  const lines: string[] = [
    'DSH Box container context (authoritative, structured):',
    '- container.id = ' + field(c.id, '<unknown>'),
    '- container.name = ' + field(c.name, '<unknown>'),
    '- container.version = ' + field(c.version, '<unknown>'),
    '- container.profile = ' + field(c.profile, '<unknown>'),
    '- paths.workspace = ' + field(p.workspace, '<unknown>'),
    '- paths.profileHome = ' + field(p.profileHome, '<unknown>'),
    '- paths.plugins = ' + field(p.plugins, '<unknown>'),
    '- paths.skills = ' + field(p.skills, '<unknown>'),
    '- paths.logs = ' + field(p.logs, '<unknown>'),
    '- paths.dshboxHome = ' + field(p.dshboxHome, '<unknown>'),
    '- paths.dshboxCli = ' + field(p.dshboxCli, '<unknown>') + ' (use this path if `dshbox` is not in PATH)',
    '- credentials.providers = ' + JSON.stringify(providers),
    '',
    'Constraint: project and creation-mode changes stay in paths.workspace; profile / plugin / skill changes stay inside this Container; do not modify another Container or system paths unless the user explicitly asks.',
  ];
  return lines.join('\n');
}

/**
 * Register the plugin against a freshly mounted Cordis context.
 *
 * Registers a single dsh-box:container system-prompt context section
 * that re-reads and re-renders the snapshot every time the agent loop
 * calls ctx.systemPrompt.assemble(). A fs.watch listener plus a
 * 30-second polling fallback keeps the snapshot current without
 * requiring a container restart.
 */
export function apply(ctx: Context, config: Config): void {
  let snapshot = loadSnapshot(config.contextFile);

  ctx.systemPrompt.context({
    name: 'dsh-box:container',
    order: config.order ?? 130,
    text: () => renderSnapshot(snapshot),
  });

  // fs.watch is unreliable on some platforms (macOS FSEvents quirks,
  // Windows ReadDirectoryChangesW with editors that use atomic rename).
  // Watcher failure or absence falls back to a 30s polling reload.
  let watcher: { close: () => void } | null = null;
  try {
    watcher = watch(config.contextFile, { persistent: false }, () => {
      snapshot = loadSnapshot(config.contextFile);
    });
  } catch {
    watcher = null;
  }
  if (watcher === null) {
    const interval = setInterval(() => {
      snapshot = loadSnapshot(config.contextFile);
    }, 30_000);
    interval.unref?.();
    ctx.effect(() => () => clearInterval(interval), 'dsh-box-context.polling-fallback()');
  } else {
    ctx.effect(() => () => watcher.close(), 'dsh-box-context.watcher()');
  }
}

export default { name, inject, Config, apply };
