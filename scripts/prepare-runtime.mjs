import { spawnSync } from 'node:child_process'

const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

const result = spawnSync('cargo', ['run', '--manifest-path', 'src-tauri/Cargo.toml', '-p', 'runtime-packager', '--', '--target', target], { stdio: 'inherit' })
process.exit(result.status ?? 1)
