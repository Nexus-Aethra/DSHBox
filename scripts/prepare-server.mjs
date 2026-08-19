import { copyFile, mkdir, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'
import { hashInputs, isFresh } from './mtime.mjs'

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${target.startsWith('win-') ? '.exe' : ''}`)

const force = process.argv.includes('--force')

// Cheap path: skip when the binary on disk is newer than every crate
// source. Cargo rewrites file mtimes whenever it links, so this is
// usually right after a rebuild — but `cargo build` of unrelated crates
// inside the workspace still invalidates `crates/`'s mtime without
// changing the daemon binary. The hash-cache check below catches that.
const sources = [join('src-tauri', 'crates'), join('src-tauri', 'Cargo.lock')]
if (isFresh(destination, sources, force)) {
  console.log(`server sidecar already prepared at ${destination}; pass --force to rebuild`)
  process.exit(0)
}

// Hash-cache: skip the expensive `cargo build --release` when the
// dshboxd crate sources + Cargo.lock are byte-identical to the last
// successful build. The Cargo.lock mtime flips on every workspace build
// (unrelated crates), so mtime alone would force a needless rebuild.
const cacheRoot = join('.cache', 'dsh-box', 'server')
await mkdir(cacheRoot, { recursive: true })
const cacheFile = join(cacheRoot, `${target}.sha256`)
const inputs = [
  join('src-tauri', 'Cargo.lock'),
  // dshboxd + every box-* crate it depends on transitively (foundation,
  // runtime, scheduler, state, ...). Hashing the whole crate tree would
  // be expensive — dshboxd's own sources + Cargo.lock is enough for the
  // common case where dependencies are pinned.
  join('src-tauri', 'crates', 'dshboxd', 'Cargo.toml'),
  join('src-tauri', 'crates', 'dshboxd', 'Cargo.lock'),
  join('src-tauri', 'crates', 'dshboxd', 'src', 'main.rs'),
]
if (hashInputs(cacheFile, inputs, force)) {
  if (existsSync(destination)) {
    console.log(`server sidecar inputs unchanged (hash cache hit at ${cacheFile}); skipping cargo build`)
    process.exit(0)
  }
  // Cache says "we built this before" but the binary is gone — fall
  // through to a real rebuild rather than leaving the user with nothing.
}

const windows = target.startsWith('win-')

// A running daemon locks its binary, causing `Text file busy` (os error 26)
// when tauri-build later copies resources/server/<target>/dshboxd. Stop it
// before rebuilding so the copy succeeds.
if (!windows) {
  const kill = spawnSync('pkill', ['-f', 'dshboxd'], { stdio: 'ignore' })
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

const cargoArgs = ['build', '--release', '--manifest-path', 'src-tauri/crates/dshboxd/Cargo.toml', '--bin', 'dshboxd']
if (rustTarget) cargoArgs.push('--target', rustTarget)
const result = spawnSync('cargo', cargoArgs, { stdio: 'inherit', env: windows ? { ...env, CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ?? 'x86_64-w64-mingw32-gcc' } : env })
if (result.status !== 0) process.exit(result.status ?? 1)
const copyExtension = windows ? '.exe' : ''
await mkdir(join('src-tauri', 'resources', 'server', target), { recursive: true })
await copyFile(join('src-tauri', 'target', ...(rustTarget ? [rustTarget] : []), 'release', `dshboxd${copyExtension}`), destination)
console.log(`prepared server sidecar at ${destination}`)
