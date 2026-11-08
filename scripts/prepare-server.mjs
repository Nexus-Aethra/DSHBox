import { copyFile, mkdir, utimes, writeFile } from 'node:fs/promises'
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { hashInputs, isFresh } from './mtime.mjs'

// Extract every `path = "../<crate>"` reference from dshboxd's Cargo.toml
// so we can scope the mtime gate to crates that actually feed the daemon.
// Hash-mismatches in unrelated crates fall through to the hash cache below.
// Resolve every path dependency of a manifest to a directory that really holds a
// Cargo.toml. Path-based on purpose: the crates live in layer directories
// (core/, domain/, edge/, apps/), so a name-shaped guess like crates/<name>
// would stop matching after a move -- and then the mtime gate below would watch
// nothing at all while still reporting success.
function readLocalDeps(manifestPath) {
  const text = readFileSync(manifestPath, 'utf8')
  const manifestDir = dirname(manifestPath)
  const deps = new Set()
  const re = /path\s*=\s*"([^"]+)"/g
  let match
  while ((match = re.exec(text)) !== null) {
    const resolved = resolve(manifestDir, match[1])
    // Skips a [[bin]] path such as src/main.rs: that is a file, not a crate root.
    if (!existsSync(join(resolved, 'Cargo.toml'))) continue
    deps.add(resolved)
  }
  // Always include the crate's own directory.
  deps.add(manifestDir)
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
const dshboxdManifest = join('src-tauri', 'crates', 'apps', 'dshboxd', 'Cargo.toml')
const localDeps = readLocalDeps(dshboxdManifest)
const sources = [
  ...localDeps,
  // The helper is staged by this script too, so its sources must count as
  // staleness for both artifacts. Without this, editing only the helper left the
  // previously staged copy in every installer -- the exact drift that building
  // the two together exists to prevent.
  join('src-tauri', 'crates', 'apps', 'box-helper'),
  // Profiles, features and workspace dependencies live here; a change to any of
  // them changes both artifacts, and neither gate used to look at this file.
  join('src-tauri', 'Cargo.toml'),
  join('src-tauri', 'Cargo.lock'),
]

// The mtime gate above only decides whether asking the hash cache is worth it;
// the cache decides whether work is really needed. The invariant that has to
// hold is one-directional: **every hash input must also be watched by mtime**.
// Then any real change trips the gate first, so the digest is never the only
// thing standing between a user and a stale artifact. The reverse is deliberate
// -- the dependency crates are watched for mtime but not hashed, because
// hashing them would cost more than the rebuild it might save. Hashing a file
// nobody watches is what would be silent: a change there would not even be
// looked at.
const cacheRoot = join('.cache', 'dsh-box', 'server')
await mkdir(cacheRoot, { recursive: true })
const cacheFile = join(cacheRoot, `${target}.sha256`)
const inputs = [
  join('src-tauri', 'Cargo.toml'),
  join('src-tauri', 'Cargo.lock'),
  dshboxdManifest,
  join('src-tauri', 'crates', 'apps', 'dshboxd', 'build.rs'),
  ...walkRustFiles(join('src-tauri', 'crates', 'apps', 'dshboxd', 'src')),
  join('src-tauri', 'crates', 'apps', 'box-helper', 'Cargo.toml'),
  ...walkRustFiles(join('src-tauri', 'crates', 'apps', 'box-helper', 'src')),
]

if (!isFresh(destination, sources, force) || !isFresh(helperDestination, sources, force)) {
  // Stale → rebuild. Record the digest *before* compiling, so it describes the
  // inputs that are about to be built: written afterwards (as it was on the miss
  // path below) the next unchanged run misses again, and every real edit cost two
  // full rebuilds instead of one.
  hashInputs(cacheFile, inputs, true)
  await rebuild(destination, target, force)
  process.exit(0)
}
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
    // `-x` matches the process *name*, not the whole command line: `-f` with a
    // pattern would also match this very build (`cargo build ... -p dshboxd -p
    // box-helper`) and kill it. `pkill` exits 1 when nothing matched, so "nothing
    // to stop" is a status; a null one means the tool did not run at all, which
    // is when claiming there is nothing running would be a guess.
    let stopped = 0
    let unavailable = false
    for (const name of ['dshboxd', 'box-helper']) {
      const kill = spawnSync('pkill', ['-x', name], { stdio: 'ignore' })
      if (kill.error) unavailable = true
      else if (kill.status === 0) stopped += 1
    }
    if (unavailable) {
      console.log('pkill unavailable; stop a running dshboxd or box-helper yourself')
    } else if (stopped === 0) {
      console.log('no running dshboxd or box-helper to stop')
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
  // Take the stamp before compiling, so that a file edited *during* the build is
  // newer than what we are about to stage and the next run rebuilds. Taken after
  // the copy, that window would be invisible.
  const stagedAt = new Date()
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
  await utimes(destination, stagedAt, stagedAt)
  await utimes(helperDestination, stagedAt, stagedAt)
  console.log(`prepared node helper at ${helperDestination}`)
}