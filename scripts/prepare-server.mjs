import { copyFile, mkdir, writeFile } from 'node:fs/promises'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'
import { isFresh } from './mtime.mjs'

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${target.startsWith('win-') ? '.exe' : ''}`)

// Skip rebuilding while the sidecar is newer than every tracked source
// (all box-* crates plus Cargo.lock). Pass --force to rebuild from scratch.
const force = process.argv.includes('--force')
const sources = [join('src-tauri', 'crates'), join('src-tauri', 'Cargo.lock')]
if (isFresh(destination, sources, force)) {
  console.log(`server sidecar already prepared at ${destination}; pass --force to rebuild`)
  process.exit(0)
}

const rustTarget = env.DSH_BOX_RUST_TARGET
const windows = target.startsWith('win-')

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
