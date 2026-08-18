import { spawnSync } from 'node:child_process'

const environment = {
  ...process.env,
  DSH_BOX_RUNTIME_TARGET: 'win-x64',
  DSH_BOX_RUST_TARGET: 'x86_64-pc-windows-gnu',
  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: 'x86_64-w64-mingw32-gcc',
}

for (const [command, args] of [
  ['pnpm', ['plugin:build']],
  ['pnpm', ['runtime:prepare']],
  ['pnpm', ['server:prepare']],
  ['pnpm', ['tauri', 'build', '--bundles', 'nsis', '--target', 'x86_64-pc-windows-gnu', '--config', 'src-tauri/tauri.windows.conf.json']],
]) {
  const result = spawnSync(command, args, { stdio: 'inherit', env: environment, shell: true })
  if (result.status !== 0) process.exit(result.status ?? 1)
}