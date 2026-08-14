import { copyFile, mkdir } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${target.startsWith('win-') ? '.exe' : ''}`)

// Skip rebuilding when the sidecar already exists (fast iteration).
// Pass --force to rebuild from scratch.
if (existsSync(destination) && !process.argv.includes('--force')) {
  console.log(`server sidecar already prepared at ${destination}; pass --force to rebuild`)
  process.exit(0)
}

const rustTarget = env.DSH_BOX_RUST_TARGET
const windows = target.startsWith('win-')
const cargoArgs = ['build', '--release', '--manifest-path', 'src-tauri/crates/dshboxd/Cargo.toml', '--bin', 'dshboxd']
if (rustTarget) cargoArgs.push('--target', rustTarget)
const result = spawnSync('cargo', cargoArgs, { stdio: 'inherit', env: windows ? { ...env, CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ?? 'x86_64-w64-mingw32-gcc' } : env })
if (result.status !== 0) process.exit(result.status ?? 1)
const copyExtension = windows ? '.exe' : ''
await mkdir(join('src-tauri', 'resources', 'server', target), { recursive: true })
await copyFile(join('src-tauri', 'target', ...(rustTarget ? [rustTarget] : []), 'release', `dshboxd${copyExtension}`), destination)
console.log(`prepared server sidecar at ${destination}`)
