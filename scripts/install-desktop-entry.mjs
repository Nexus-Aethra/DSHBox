#!/usr/bin/env node
// Install (or refresh) the Linux desktop entry for DSH Box.
//
// The CLI is argument-driven: `dshbox ui` launches the desktop GUI, while a
// bare `dshbox` prints help and exits. Tauri's generated .desktop files use
// `Exec=dshbox` (no arguments), so clicking the icon silently prints help
// and nothing opens. This script writes a correct entry whose Exec always
// ends with `ui`.
//
// Default target is the per-user directory (~/.local/share/applications),
// which takes precedence over the system-wide /usr/share/applications entry
// and needs no privileges. Pass `--system` to write the system entry instead
// (requires root; used after `pnpm bundle:linux` installs).
//
// Binary resolution order:
//   1. $DSHBOX_BIN (explicit override)
//   2. `dshbox` on PATH (already installed, e.g. /usr/bin/dshbox)
//   3. src-tauri/target/{release,debug}/dshbox (fresh build)

import { existsSync, mkdirSync, writeFileSync, chmodSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'

const root = process.cwd()
const system = process.argv.includes('--system')

function resolveBinary() {
  if (process.env.DSHBOX_BIN) {
    const explicit = resolve(process.env.DSHBOX_BIN)
    if (existsSync(explicit)) return explicit
    console.error(`DSHBOX_BIN points at a missing file: ${explicit}`)
    process.exit(1)
  }
  const which = spawnSync('which', ['dshbox'], { encoding: 'utf8' })
  if (which.status === 0 && which.stdout.trim()) {
    return which.stdout.trim()
  }
  for (const profile of ['release', 'debug']) {
    const candidate = join(root, 'src-tauri/target', profile, 'dshbox')
    if (existsSync(candidate)) return candidate
  }
  console.error(
    'cannot locate the dshbox binary; set DSHBOX_BIN, install it on PATH, or run `pnpm bundle:linux` first'
  )
  process.exit(1)
}

const binary = resolveBinary()
const desktop = [
  '[Desktop Entry]',
  'Type=Application',
  'Name=dshbox',
  'Comment=Managed DeepSeek Harness desktop runtime',
  `Exec=${binary} ui`,
  'Icon=dshbox',
  'Terminal=false',
  'StartupWMClass=dshbox',
  'Categories=Utility;',
  '',
].join('\n')

const directory = system
  ? '/usr/share/applications'
  : join(homedir(), '.local/share/applications')
const target = join(directory, 'dshbox.desktop')

mkdirSync(directory, { recursive: true })
writeFileSync(target, desktop)
chmodSync(target, 0o644)
console.log(`wrote ${target}`)
console.log(`  Exec=${binary} ui`)
console.log(system ? 'system desktop entry installed' : 'user desktop entry installed (takes precedence over the system one)')
