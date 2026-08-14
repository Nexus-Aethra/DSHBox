import { existsSync } from 'node:fs'
import { spawnSync } from 'node:child_process'

const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

// Skip repacking when the bundled runtime already exists (fast iteration).
// Pass --force to rebuild from scratch.
const output = `src-tauri/resources/runtime/${target}`
if (existsSync(output) && !process.argv.includes('--force')) {
  console.log(`bundled runtime already prepared at ${output}; pass --force to rebuild`)
  process.exit(0)
}

const result = spawnSync('cargo', ['run', '--manifest-path', 'src-tauri/Cargo.toml', '-p', 'runtime-packager', '--', '--target', target], { stdio: 'inherit' })
process.exit(result.status ?? 1)
