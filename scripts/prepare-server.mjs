import { copyFile, mkdir, utimes, writeFile } from 'node:fs/promises'
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { dirname, join } from 'node:path'
import { hashInputs, isFresh } from './mtime.mjs'

// Extract every `path = "../<crate>"` reference from dshboxd's Cargo.toml
// so we can scope the mtime gate to crates that actually feed the daemon.
// Hash-mismatches in unrelated crates fall through to the hash cache below.
function readLocalDeps(manifestPath) {
  const text = readFileSync(manifestPath, 'utf8')
  const deps = new Set()
  const re = /path\s*=\s*"\.\.\/([\w-]+)"/g
  let match
  while ((match = re.exec(text)) !== null) deps.add(match[1])
  // Always include dshboxd's own crate directory.
  deps.add('dshboxd')
  return [...deps].sort()
}

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const copyExtension = target.startsWith('win-') ? '.exe' : ''
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${copyExtension}`)
// The node helper ships beside the daemon so a node can be handed one binary
// without a second build pipeline to version. Stage B of the node work is what
// runs it; until then this only guarantees the artifact exists.
const helperDestination = join('src-tauri', 'resources', 'helper', target, `box-helper${copyExtension}`)

const force = process.argv.includes('--force')

// mtime gate: rebuild whenever a source that actually feeds the daemon
// is newer than the existing sidecar. We only watch crates that dshboxd
// depends on (per its Cargo.toml), so an unrelated crate edit (e.g.
// box-data-scheduler, which dshboxd doesn't use) won't force a needless
// rebuild. Cross-crate edits in transitive dependencies still trip this
// because Cargo.lock + the affected crate are included.
const localDeps = readLocalDeps(join('src-tauri', 'crates', 'dshboxd', 'Cargo.toml'))
const sources = [
  ...localDeps.map((name) => join('src-tauri', 'crates', name)),
  // The helper is staged by this script too, so its sources must count as
  // staleness for both artifacts. Without this, editing only the helper left the
  // previously staged copy in every installer -- the exact drift that building
  // the two together exists to prevent.
  join('src-tauri', 'crates', 'box-helper'),
  join('src-tauri', 'Cargo.lock'),
]
if (!isFresh(destination, sources, force) || !isFresh(helperDestination, sources, force)) {
  // Stale → rebuild, skip the hash-cache short-circuit entirely.
  await rebuild(destination, target, force)
  process.exit(0)
}

// mtime says fresh. Now ask the hash cache whether the daemon's own
// sources actually changed since the last build — when they didn't,
// save a 30+ second incremental cargo invocation. When the hash matches
// but the binary is gone, fall through to a real rebuild.
const cacheRoot = join('.cache', 'dsh-box', 'server')
await mkdir(cacheRoot, { recursive: true })
const cacheFile = join(cacheRoot, `${target}.sha256`)
const inputs = [
  join('src-tauri', 'Cargo.lock'),
  join('src-tauri', 'crates', 'dshboxd', 'Cargo.toml'),
  ...walkRustFiles(join('src-tauri', 'crates', 'dshboxd', 'src')),
  join('src-tauri', 'crates', 'box-helper', 'Cargo.toml'),
  ...walkRustFiles(join('src-tauri', 'crates', 'box-helper', 'src')),
]
if (hashInputs(cacheFile, inputs, force) && existsSync(destination) && existsSync(helperDestination)) {
  console.log(`server sidecar inputs unchanged (hash cache hit at ${cacheFile}); skipping cargo build`)
  process.exit(0)
}
await rebuild(destination, target, force)

function walkRustFiles(dir) {
  const files = []
  try {
    for (const entry of readdirSync(dir)) {
      const p = join(dir, entry)
      const st = statSync(p)
      if (st.isDirectory()) files.push(...walkRustFiles(p))
      else if (entry.endsWith('.rs')) files.push(p)
    }
  } catch { /* skip missing dirs */ }
  return files
}

async function rebuild(destination, target, force) {
  const windows = target.startsWith('win-')

  // A running daemon locks its binary, causing `Text file busy` (os error 26)
  // when tauri-build later copies resources/server/<target>/dshboxd. Stop it
  // before rebuilding so the copy succeeds.
  if (!windows) {
    // The helper will need the same treatment once something runs it on this
    // host. On Windows neither binary can be replaced while it is running
    // (EBUSY), which is why the stop lives in the Unix branch.
    const kill = spawnSync('pkill', ['-f', 'dshboxd|box-helper'], { stdio: 'ignore' })
    if (kill.status === null) {
      console.log('no running dshboxd to stop')
    }
  }

  const rustTarget = env.DSH_BOX_RUST_TARGET

  // Every real rebuild starts a new build batch: stamp it with the epoch
  // seconds so the daemon and the desktop client embedded in the same batch
  // can be matched over RPC (stale daemons are restarted by the client).
  const stamp = String(Math.floor(Date.now() / 1000))
  await writeFile(join('src-tauri', '.build-stamp'), `${stamp}\n`)
  console.log(`build stamp: ${stamp}`)

  // One cargo invocation for both artifacts: they share the dependency graph,
  // and building them separately would compile it twice.
  // `-p` selects each package's default targets, which is one binary each today
  // (dshboxd/Cargo.toml and box-helper/Cargo.toml declare a single `[[bin]]`) --
  // that equality is the precondition for using `-p` instead of naming the bins.
  // Naming them instead (`--bin dshboxd --bin box-helper`) does not work here:
  // without a `-p` to scope it, cargo looks the name up in the default-run
  // package and fails with "no bin target named `dshboxd`".
  const cargoArgs = ['build', '--release', '--manifest-path', 'src-tauri/Cargo.toml', '-p', 'dshboxd', '-p', 'box-helper']
  if (rustTarget) cargoArgs.push('--target', rustTarget)
  const result = spawnSync('cargo', cargoArgs, { stdio: 'inherit', env: windows ? { ...env, CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ?? 'x86_64-w64-mingw32-gcc' } : env })
  if (result.status !== 0) process.exit(result.status ?? 1)
  const binDir = join('src-tauri', 'target', ...(rustTarget ? [rustTarget] : []), 'release')
  await mkdir(join('src-tauri', 'resources', 'server', target), { recursive: true })
  await copyFile(join(binDir, `dshboxd${copyExtension}`), destination)
  console.log(`prepared server sidecar at ${destination}`)
  await mkdir(join('src-tauri', 'resources', 'helper', target), { recursive: true })
  await copyFile(join(binDir, `box-helper${copyExtension}`), helperDestination)
  // Stamp both staged copies with "now". `copyFile` keeps the source mtime and
  // cargo only relinks a binary whose own sources changed, so a staged artifact
  // would stay older than some watched file forever: the mtime gate above would
  // be false on every run and the hash-cache shortcut unreachable, turning every
  // `server:prepare` into a full rebuild. The gate only answers "is anything
  // newer than what I staged"; the hash cache is what decides real work.
  const stagedAt = new Date()
  await utimes(destination, stagedAt, stagedAt)
  await utimes(helperDestination, stagedAt, stagedAt)
  console.log(`prepared node helper at ${helperDestination}`)
}