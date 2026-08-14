import { copyFile, mkdir } from 'node:fs/promises'
import { arch, platform } from 'node:process'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'

const target = process.env.DSH_BOX_RUNTIME_TARGET ?? ({ linux: { x64: 'linux-x64', arm64: 'linux-arm64' }, win32: { x64: 'win-x64', arm64: 'win-arm64' }, darwin: { x64: 'macos-x64', arm64: 'macos-arm64' } }[platform]?.[arch])
if (!target) throw new Error(`unsupported server target: ${platform}-${arch}`)
const result = spawnSync('cargo', ['build', '--release', '--manifest-path', 'src-tauri/Cargo.toml', '--bin', 'dshboxd'], { stdio: 'inherit' })
if (result.status !== 0) process.exit(result.status ?? 1)
const extension = platform === 'win32' ? '.exe' : ''
const destination = join('src-tauri', 'resources', 'server', target, `dshboxd${extension}`)
await mkdir(join('src-tauri', 'resources', 'server', target), { recursive: true })
await copyFile(join('src-tauri', 'target', 'release', `dshboxd${extension}`), destination)
console.log(`prepared server sidecar at ${destination}`)
