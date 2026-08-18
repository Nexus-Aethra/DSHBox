// Shared freshness check for the resource-prepare scripts.
//
// Each prepare script (server/runtime/plugin) skips rebuilding its output
// when the output is newer than every tracked source file, so packaging
// stays fast on unchanged trees while a source edit automatically
// invalidates the cached artifact. `--force` always rebuilds.

import { existsSync, readdirSync, statSync } from 'node:fs'
import { join } from 'node:path'

// Directories that never influence build output freshness.
const SKIP_DIRS = new Set(['node_modules', 'dist', 'target', '.git', '.pnpm', '.vite'])

function latestMtime(path) {
  const stat = statSync(path)
  if (!stat.isDirectory()) return stat.mtimeMs
  let latest = stat.mtimeMs
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    if (entry.isDirectory() && SKIP_DIRS.has(entry.name)) continue
    latest = Math.max(latest, latestMtime(join(path, entry.name)))
  }
  return latest
}

// Return true when `output` exists and is newer than every `sources` entry.
// A missing or unreadable path counts as stale (rebuild).
export function isFresh(output, sources, force) {
  if (force) return false
  let outputMtime
  try {
    outputMtime = latestMtime(output)
  } catch {
    return false
  }
  for (const source of sources) {
    try {
      if (latestMtime(source) > outputMtime) return false
    } catch {
      return false
    }
  }
  return true
}

// True when every entry of `paths` exists on disk.
export function allExist(paths) {
  return paths.every((path) => existsSync(path))
}
