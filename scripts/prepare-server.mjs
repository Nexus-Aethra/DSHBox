import { copyFile, mkdir } from 'node:fs/promises'
import { arch, env, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const rustTarget = env.DSH_BOX_RUST_TARGET
const windows = target.startsWith('win-')
const cargoArgs = ['build', '--release', '--manifest-path', 'src-tauri/crates/dshboxd/Cargo.toml', '--bin', 'dshboxd']
if (rustTarget) cargoArgs.push('--target', rustTarget)
const result = spawnSync('cargo', cargoArgs, { stdio: 'inherit', env: windows ? { ...env, CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ?? 'x86_64-w64-mingw32-gcc' } : env })
if (result.status !== 0) process.exit(result.status ?? 1)
const extension = windows ? '.exe' : ''
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${extension}`)
await mkdir(join('src-tauri', 'resources', 'server', target), { recursive: true })
await copyFile(join('src-tauri', 'target', ...(rustTarget ? [rustTarget] : []), 'release', `dshboxd${extension}`), destination)
console.log(`prepared server sidecar at ${destination}`)
