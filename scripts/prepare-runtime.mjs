import { spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { hashInputs, isFresh } from './mtime.mjs'

const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

const output = `src-tauri/resources/runtime/${target}`
const force = process.argv.includes('--force')

// Cheap path: if the bundled runtime dir already exists and is newer than
// every tracked source, skip the whole packager invocation. This is the
// common case during incremental development (Cargo.lock mtime drift,
// sibling crate edits, etc.).
const sources = [join('src-tauri', 'tools', 'runtime-packager'), join('src-tauri', 'Cargo.lock')]
if (isFresh(output, sources, force)) {
  console.log(`bundled runtime already prepared at ${output}; pass --force to rebuild`)
  process.exit(0)
}

// Stronger path: hash the lockfile + the packager's manifest + the
// packager source. Skip even the mtime check when the content hash is
// identical — this catches the case where `cargo build` rewrote Cargo.lock
// without changing the lock entries the packager reads. The packager has
// no Cargo.lock of its own until it has been built once, so we omit it
// from the hash (caller verifies with `isFresh` if needed).
const cacheRoot = join('.cache', 'dsh-box', 'runtime')
mkdirSync(cacheRoot, { recursive: true })
const cacheFile = join(cacheRoot, `${target}.sha256`)
const inputs = [
  'runtime-lock.json',
  join('src-tauri', 'tools', 'runtime-packager', 'Cargo.toml'),
  join('src-tauri', 'tools', 'runtime-packager', 'src', 'main.rs'),
]
if (hashInputs(cacheFile, inputs, force)) {
  console.log(`bundled runtime inputs unchanged (hash cache hit at ${cacheFile}); skipping extract`)
  // The output may be missing if a previous run failed mid-extract. Touch
  // a marker file so `isFresh` considers the bundle up to date.
  if (!existsSync(output)) {
    mkdirSync(output, { recursive: true })
    writeMarker(output, target)
  }
  process.exit(0)
}

// Fallback: re-run the packager. Downloads are already cached inside
// `dirs::cache_dir()` by runtime-packager itself, so this is mostly
// archive-extract time + the packager binary's own cargo invocation.
const result = spawnSync(
  'cargo',
  ['run', '--manifest-path', 'src-tauri/Cargo.toml', '-p', 'runtime-packager', '--', '--target', target],
  { stdio: 'inherit' },
)
if (result.status === 0) {
  writeMarker(output, target)
}
process.exit(result.status ?? 1)

function writeMarker(outputDir, target) {
  const marker = join(outputDir, '.prepared')
  writeFileSync(marker, `target=${target}\nlocked-at=${new Date().toISOString()}\n`)
}
