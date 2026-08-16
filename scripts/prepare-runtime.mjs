import { spawnSync } from 'node:child_process'
import { join } from 'node:path'
import { isFresh } from './mtime.mjs'

const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

// Skip repacking while the bundled runtime is newer than every tracked
// source (the packager crate plus Cargo.lock). Pass --force to rebuild.
const output = `src-tauri/resources/runtime/${target}`
const force = process.argv.includes('--force')
const sources = [join('src-tauri', 'tools', 'runtime-packager'), join('src-tauri', 'Cargo.lock')]
if (isFresh(output, sources, force)) {
  console.log(`bundled runtime already prepared at ${output}; pass --force to rebuild`)
  process.exit(0)
}

const result = spawnSync('cargo', ['run', '--manifest-path', 'src-tauri/Cargo.toml', '-p', 'runtime-packager', '--', '--target', target], { stdio: 'inherit' })
process.exit(result.status ?? 1)
