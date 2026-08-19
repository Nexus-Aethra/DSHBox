// Shared freshness check for the resource-prepare scripts.
//
// Each prepare script (server/runtime/plugin) skips rebuilding its output
// when the output is newer than every tracked source file, so packaging
// stays fast on unchanged trees while a source edit automatically
// invalidates the cached artifact. `--force` always rebuilds.

import { createHash } from 'node:crypto'
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs'
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

// Content-hash cache: lets `prepare-runtime.mjs` skip re-extraction when
// the lock file's URL/integrity tuple is unchanged, even when other files
// in the repo (Cargo.lock, runtime-packager source) have been touched.
//
// Layout: `<cache>/<key>.sha256` holds the SHA-256 of every input file at
// the time of the most recent successful run. A run is "fresh" when the
// recomputed hash matches the stored one.
//
// Inputs that don't exist on disk are skipped silently — the packager's
// own Cargo.lock doesn't appear until its first build, for example. As
// long as at least one input hashes, a missing file just contributes
// nothing to the digest (it is not a cache miss by itself).
export function hashInputs(cacheFile, inputs, force) {
  const hasher = createHash('sha256')
  let hashedAny = false
  for (const input of inputs) {
    try {
      hasher.update(readFileSync(input))
      hashedAny = true
    } catch {
      // Optional input; skip rather than invalidate.
    }
  }
  if (!hashedAny) return false
  const current = hasher.digest('hex')
  if (force) {
    writeFileSync(cacheFile, current)
    return false
  }
  let previous
  try {
    previous = readFileSync(cacheFile, 'utf8').trim()
  } catch {
    previous = ''
  }
  if (previous === current) return true
  writeFileSync(cacheFile, current)
  return false
}
