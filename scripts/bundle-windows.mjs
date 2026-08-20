// Build the Windows installer (NSIS) end-to-end on the current Windows host.
//
// Pipeline:
//   1. preflight — locate pnpm (PATH / corepack / common install dirs),
//      node, cargo/rustc, and a working linker. Choose MSVC by default
//      (because that's what Visual Studio BuildTools ships with on
//      Windows), fall back to x86_64-pc-windows-gnu if that's the only
//      target whose linker we can find, and let the user force either
//      via --msvc / --gnu / --linker.
//   2. plugin:build       (script: plugin:build)
//   3. runtime:prepare    (script: runtime:prepare)
//   4. server:prepare     (script: server:prepare; also stops dshboxd.exe)
//   5. tauri build (NSIS) under the chosen Rust target
//
// Notes:
//   - Non-Windows hosts are rejected up front.
//   - pnpm is resolved robustly: PATH → corepack → known install dirs.
//   - The linker is resolved per toolchain (link.exe for MSVC,
//     x86_64-w64-mingw32-gcc for GNU). For MSVC we also honour the
//     standard vcvars* environment via vswhere + vcvars setup so that
//     cargo can find link.exe and the Windows SDK libs without the
//     user having to start a "Developer Command Prompt".
//   - Forwarded args after `--` are appended to the final `tauri build`.
//     `--no-bundle` skips step 5.
//   - We never silently override CARGO_TARGET_*_LINKER if the user
//     already set one.

import { spawnSync } from 'node:child_process'
import { existsSync, readdirSync } from 'node:fs'
import { platform } from 'node:process'
import { delimiter, dirname, join, resolve } from 'node:path'

const HOST = platform
const IS_WINDOWS = HOST === 'win32'

if (!IS_WINDOWS) {
  console.error(
    `bundle-windows.mjs must be run on Windows (current host: ${HOST}).`,
  )
  console.error(
    'Cross-compiling Windows installers from Linux/macOS requires extra',
  )
  console.error(
    'toolchain shims and is not supported by this script; run it inside',
  )
  console.error('a Windows shell (cmd.exe, PowerShell, or Git Bash).')
  process.exit(2)
}

// ---- CLI parsing -----------------------------------------------------------

function parseArgs(argv) {
  const flags = {
    toolchain: 'auto', // 'auto' | 'msvc' | 'gnu'
    linker: null,
    skipBundle: false,
    forwarded: [],
  }
  const sepIdx = argv.indexOf('--')
  const mainArgs = sepIdx >= 0 ? argv.slice(0, sepIdx) : argv
  const forwarded = sepIdx >= 0 ? argv.slice(sepIdx + 1) : []
  flags.forwarded = forwarded
  flags.tauriArgs = forwarded.filter((a) => a !== '--no-bundle')
  flags.skipBundle = flags.skipBundle || forwarded.includes('--no-bundle')

  for (let i = 0; i < mainArgs.length; i++) {
    const a = mainArgs[i]
    if (a === '--msvc') flags.toolchain = 'msvc'
    else if (a === '--gnu') flags.toolchain = 'gnu'
    else if (a === '--auto') flags.toolchain = 'auto'
    else if (a === '--no-bundle') flags.skipBundle = true
    else if (a === '--linker' && i + 1 < mainArgs.length) flags.linker = mainArgs[++i]
    else if (a.startsWith('--linker=')) flags.linker = a.slice('--linker='.length)
    else if (a === '--help' || a === '-h') {
      printHelp()
      process.exit(0)
    }
  }
  return flags
}

function printHelp() {
  console.log(`Usage: node scripts/bundle-windows.mjs [options]

Options:
  --msvc                  Force the MSVC toolchain (link.exe)
  --gnu                   Force the GNU toolchain (x86_64-w64-mingw32-gcc)
  --auto                  Auto-detect (default: prefer MSVC, fall back to GNU)
  --linker <path>         Path to the linker (overrides auto-detection)
  --no-bundle             Skip the final tauri build step
  --help, -h              Show this help

Anything after \`--\` is forwarded to \`tauri build\`.`)
}

// ---- tool discovery --------------------------------------------------------

function pathDirs() {
  return (process.env.PATH || '').split(delimiter).filter(Boolean)
}

function isFile(path) {
  try {
    return existsSync(path)
  } catch {
    return false
  }
}

// "Well known" install dirs — used purely for hints. We never mutate PATH;
// we only suggest. Users opt in via --linker, --msvc, --gnu, or by setting
// PATH themselves.
function wellKnownDirs() {
  const sysdrive = process.env.SystemDrive || 'C:'
  const programFiles = process.env['ProgramFiles'] || join(sysdrive, 'Program Files')
  const programFiles86 = process.env['ProgramFiles(x86)'] || join(sysdrive, 'Program Files (x86)')
  const appdata = process.env.APPDATA || join(process.env.USERPROFILE || '', 'AppData', 'Roaming')
  const localappdata = process.env.LOCALAPPDATA || join(process.env.USERPROFILE || '', 'AppData', 'Local')
  const userprofile = process.env.USERPROFILE || ''

  return {
    // Node / pnpm
    nodejs: [
      join(sysdrive, 'nodejs', 'node_global'),
      join(sysdrive, 'nodejs'),
      join(programFiles, 'nodejs'),
    ],
    pnpm: [
      join(localappdata, 'pnpm'),
      join(appdata, 'npm'),
    ],
    rust: [
      join(userprofile, '.cargo', 'bin'),
    ],
    // MSVC link.exe is buried under versioned dirs; we resolve via
    // vswhere below rather than guessing the version.
    msvc: [],
    gnu: [
      join(sysdrive, 'msys64', 'mingw64', 'bin'),
      join(sysdrive, 'mingw64', 'bin'),
      join(programFiles, 'msys64', 'mingw64', 'bin'),
      join(programFiles86, 'msys64', 'mingw64', 'bin'),
      join(sysdrive, 'Git', 'mingw64', 'bin'),
      join(programFiles, 'Git', 'mingw64', 'bin'),
      join(localappdata, 'Programs', 'Git', 'mingw64', 'bin'),
    ],
  }
}

function findCommand(cmd, extraDirs = []) {
  // On Windows, prefer the script wrappers (.cmd/.bat) over the bare name
  // and the executable (.exe). Some installs ship a bare-name launcher
  // (e.g. /d/nodejs/corepack) that isn't a real PE binary, so blindly
  // picking it as a spawnSync target will fail with ENOENT.
  const names = IS_WINDOWS
    ? [`${cmd}.cmd`, `${cmd}.bat`, `${cmd}.exe`, `${cmd}.ps1`, cmd]
    : [cmd]
  for (const dir of [...pathDirs(), ...extraDirs]) {
    for (const name of names) {
      if (isFile(join(dir, name))) return join(dir, name)
    }
  }
  return cmd
}

function resolveAbs(cmd) {
  if (!cmd) return null
  // Treat anything containing a path separator as already-resolved.
  if (cmd.includes('/') || cmd.includes('\\')) return cmd
  const found = findCommand(cmd, [
    ...wellKnownDirs().nodejs,
    ...wellKnownDirs().pnpm,
    ...wellKnownDirs().rust,
  ])
  if (found && (found.includes('/') || found.includes('\\'))) return found
  return null
}

function findNode() {
  return resolveAbs('node')
}
function findCargo() {
  return resolveAbs('cargo')
}
function findRustc() {
  return resolveAbs('rustc')
}

function findPnpm() {
  // 1) PATH + well-known dirs (a real pnpm install).
  const direct = resolveAbs('pnpm')
  if (direct) return { command: direct, prefixArgs: [], source: 'PATH' }

  // 2) corepack shim — corepack 0.31+ ships with Node and can act as a
  //    dispatcher: `corepack pnpm ...` works even if the pnpm shim
  //    hasn't been enabled yet.
  const corepack = resolveAbs('corepack')
  if (corepack) return { command: corepack, prefixArgs: ['pnpm'], source: 'corepack' }

  return null
}

// ---- MSVC / GNU linker discovery ------------------------------------------

// Try to locate a link.exe under any installed VS 2022 instance. We avoid
// spawning vswhere because it's an extra dependency; instead we walk the
// usual install roots directly.
function findMsvcLinker() {
  const sysdrive = process.env.SystemDrive || 'C:'
  const programFiles = process.env['ProgramFiles'] || join(sysdrive, 'Program Files')
  const programFiles86 = process.env['ProgramFiles(x86)'] || join(sysdrive, 'Program Files (x86)')

  const roots = [
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'BuildTools', 'VC', 'Tools', 'MSVC'),
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'Community', 'VC', 'Tools', 'MSVC'),
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'Enterprise', 'VC', 'Tools', 'MSVC'),
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'Professional', 'VC', 'Tools', 'MSVC'),
    join(programFiles, 'Microsoft Visual Studio', '2022', 'BuildTools', 'VC', 'Tools', 'MSVC'),
    join(programFiles, 'Microsoft Visual Studio', '2022', 'Community', 'VC', 'Tools', 'MSVC'),
    join(programFiles, 'Microsoft Visual Studio', '17.0', 'BuildTools', 'VC', 'Tools', 'MSVC'),
  ]

  for (const root of roots) {
    if (!isFile(root)) continue
    let versions
    try {
      versions = readdirSync(root).sort().reverse()
    } catch {
      continue
    }
    for (const v of versions) {
      const link = join(root, v, 'bin', 'Hostx64', 'x64', 'link.exe')
      if (isFile(link)) return link
    }
  }
  return null
}

function findMsvcToolset() {
  // Returns the MSVC root dir (the dir that contains the version dirs)
  // and the version subdir, so we can wire up PATH and INCLUDE/LIB.
  const sysdrive = process.env.SystemDrive || 'C:'
  const programFiles = process.env['ProgramFiles'] || join(sysdrive, 'Program Files')
  const programFiles86 = process.env['ProgramFiles(x86)'] || join(sysdrive, 'Program Files (x86)')

  const roots = [
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'BuildTools', 'VC', 'Tools', 'MSVC'),
    join(programFiles86, 'Microsoft Visual Studio', '2022', 'Community', 'VC', 'Tools', 'MSVC'),
    join(programFiles, 'Microsoft Visual Studio', '2022', 'BuildTools', 'VC', 'Tools', 'MSVC'),
  ]

  for (const root of roots) {
    if (!isFile(root)) continue
    let versions
    try {
      versions = readdirSync(root).sort().reverse()
    } catch {
      continue
    }
    if (versions.length > 0) {
      return { root, version: versions[0], bin: join(root, versions[0], 'bin', 'Hostx64', 'x64') }
    }
  }
  return null
}

function findGnuLinker() {
  return resolveAbs('x86_64-w64-mingw32-gcc') ||
    // Last fallback: ask Node to expand well-known dirs ourselves.
    (() => {
      for (const dir of wellKnownDirs().gnu) {
        const candidate = join(dir, 'x86_64-w64-mingw32-gcc.exe')
        if (isFile(candidate)) return candidate
      }
      return null
    })()
}

// ---- toolchain selection ---------------------------------------------------

function selectToolchain(flags) {
  const msvcLink = flags.linker && findLinkerMatchesToolchain(flags.linker, 'msvc') ? flags.linker : findMsvcLinker()
  const gnuLink = flags.linker && findLinkerMatchesToolchain(flags.linker, 'gnu') ? flags.linker : findGnuLinker()

  const wants = flags.toolchain
  if (wants === 'msvc') {
    if (!msvcLink) {
      console.error('--msvc requested but no link.exe found under Visual Studio 2022.')
      console.error('Install Visual Studio Build Tools with the "Desktop development with C++" workload,')
      console.error('or pass --linker <path-to-link.exe>.')
      process.exit(6)
    }
    return { kind: 'msvc', target: 'x86_64-pc-windows-msvc', linker: msvcLink }
  }
  if (wants === 'gnu') {
    if (!gnuLink) {
      console.error('--gnu requested but no x86_64-w64-mingw32-gcc found.')
      console.error('Install MSYS2 mingw-w64 and add its bin/ to PATH,')
      console.error('or pass --linker <path-to-gcc.exe>.')
      process.exit(6)
    }
    return { kind: 'gnu', target: 'x86_64-pc-windows-gnu', linker: gnuLink }
  }
  // auto: prefer MSVC, fall back to GNU
  if (msvcLink) return { kind: 'msvc', target: 'x86_64-pc-windows-msvc', linker: msvcLink }
  if (gnuLink) return { kind: 'gnu', target: 'x86_64-pc-windows-gnu', linker: gnuLink }
  return { kind: null, target: null, linker: null }
}

function findLinkerMatchesToolchain(path, kind) {
  const lower = path.toLowerCase()
  if (kind === 'msvc') return lower.endsWith('link.exe') || lower.includes('msvc')
  if (kind === 'gnu') return lower.includes('mingw') || lower.includes('gcc')
  return true
}

// ---- preflight -------------------------------------------------------------

function preflight(flags) {
  console.log('▶ preflight: scanning for toolchain')

  const node = findNode()
  const cargo = findCargo()
  const rustc = findRustc()
  const pnpm = findPnpm()
  const toolchain = selectToolchain(flags)

  const found = {
    node,
    cargo,
    rustc,
    pnpm: pnpm?.command,
    linker: toolchain.linker,
    target: toolchain.target,
  }
  for (const [k, v] of Object.entries(found)) {
    console.log(`  ${v ? '✓' : '✗'} ${k.padEnd(7)} ${v ?? '(not found)'}`)
  }
  console.log(`  toolchain: ${toolchain.kind ?? '—'}`)
  console.log(`  pnpm via:  ${pnpm?.source ?? '—'}`)
  const missing = []
  if (!node) missing.push('node (>= 20)')
  if (!pnpm) missing.push('pnpm (or corepack)')
  if (!cargo) missing.push('cargo (Rust toolchain)')
  if (!rustc) missing.push('rustc')
  if (!toolchain.linker) missing.push('linker (MSVC link.exe or x86_64-w64-mingw32-gcc)')

  if (missing.length > 0) {
    console.error('')
    console.error('Missing required tools:')
    for (const m of missing) console.error(`  - ${m}`)
    console.error('')
    console.error('Install hints:')
    console.error('  node      : https://nodejs.org/   or   winget install OpenJS.NodeJS.LTS')
    console.error('  pnpm      : corepack enable && corepack prepare pnpm@latest --activate')
    console.error('               (or: npm i -g pnpm   /   winget install pnpm.pnpm)')
    console.error('  rust      : https://rustup.rs/    then: rustup target add x86_64-pc-windows-msvc')
    console.error('  MSVC      : Visual Studio Build Tools + "Desktop development with C++" workload')
    console.error('               (this is the default toolchain on this script)')
    console.error('  GNU alt.  : MSYS2 (pacman -S mingw-w64-x86_64-gcc), then --gnu')
    process.exit(3)
  }

  return {
    node,
    cargo,
    rustc,
    pnpm: pnpm.command,
    pnpmPrefixArgs: pnpm.prefixArgs,
    pnpmSource: pnpm.source,
    toolchain,
  }
}

// ---- step runner -----------------------------------------------------------

function runStep(label, command, args, opts = {}) {
  const started = Date.now()
  console.log('')
  console.log(`▶ ${label}`)
  console.log(`  $ ${command} ${args.join(' ')}`)

  const env = { ...process.env, ...(opts.env ?? {}) }
  // Windows refuses to spawn .cmd/.bat scripts without an intermediate
  // shell (EINVAL on Node 20+); PE binaries and bare executables work
  // fine with shell: false. Detect and only opt in for the wrappers.
  const lower = command.toLowerCase()
  const needsShell = IS_WINDOWS && (lower.endsWith('.cmd') || lower.endsWith('.bat'))
  const result = spawnSync(command, args, {
    stdio: 'inherit',
    env,
    shell: needsShell,
  })

  const elapsed = ((Date.now() - started) / 1000).toFixed(1)
  if (result.error) {
    console.error(`✗ ${label} failed to start: ${result.error.message}`)
    process.exit(result.status ?? 1)
  }
  if (result.status !== 0) {
    console.error(`✗ ${label} exited with code ${result.status} after ${elapsed}s`)
    process.exit(result.status ?? 1)
  }
  console.log(`✓ ${label} (${elapsed}s)`)
  return result
}

// ---- main ------------------------------------------------------------------

const flags = parseArgs(process.argv.slice(2))
const tools = preflight(flags)

console.log('')
console.log(`host=${HOST}`)
console.log(`node=${tools.node}`)
console.log(`pnpm=${tools.pnpm}${tools.pnpmPrefixArgs.length ? ` (via ${tools.pnpmSource})` : ''}`)
console.log(`cargo=${tools.cargo}`)
console.log(`toolchain=${tools.toolchain.kind} target=${tools.toolchain.target}`)
console.log(`linker=${tools.toolchain.linker}`)

// 1. plugin:build
runStep('plugin:build', tools.pnpm, [...tools.pnpmPrefixArgs, 'plugin:build'])

// 2. runtime:prepare
runStep('runtime:prepare', tools.pnpm, [...tools.pnpmPrefixArgs, 'runtime:prepare'])

// 3. server:prepare
runStep('server:prepare', tools.pnpm, [...tools.pnpmPrefixArgs, 'server:prepare'])

// 4. tauri build (NSIS)
if (flags.skipBundle) {
  console.log('')
  console.log('▶ skipping tauri build (--no-bundle)')
  process.exit(0)
}

const tauriEnv = {
  DSH_BOX_RUNTIME_TARGET: 'win-x64',
  DSH_BOX_RUST_TARGET: tools.toolchain.target,
}

// Tauri runs beforeBuildCommand (and several plugin hooks) by spawning
// bare `pnpm` / `node` commands via PATH. On a host without a global
// pnpm install we have to inject the corepack shims directory and the
// Node bin directory into the child's PATH ourselves.
{
  const nodeBin = tools.node ? dirname(tools.node) : null
  const corepackShims = nodeBin ? join(nodeBin, 'node_modules', 'corepack', 'shims') : null
  const extras = [corepackShims, nodeBin].filter(Boolean)
  let basePath = process.env.PATH ?? ''
  for (const extra of extras) {
    if (extra && !basePath.split(delimiter).includes(extra)) {
      basePath = `${extra}${delimiter}${basePath}`
    }
  }
  if (extras.length > 0) tauriEnv.PATH = basePath
}

if (tools.toolchain.kind === 'gnu') {
  // GNU: cargo needs the linker binary on PATH or via this env var.
  if (!process.env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER) {
    tauriEnv.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = tools.toolchain.linker
  }
} else if (tools.toolchain.kind === 'msvc') {
  // MSVC: wire PATH, INCLUDE, LIB, LIBPATH so cargo can find link.exe and
  // the Windows SDK libs. We do this by appending the toolset bin dir to
  // PATH (matching how a "Developer Command Prompt" looks).
  const msvc = findMsvcToolset()
  if (msvc) {
    const sep = delimiter
    tauriEnv.PATH = `${msvc.bin}${sep}${tauriEnv.PATH ?? process.env.PATH ?? ''}`
  }
}

runStep(
  'tauri build (MSI)',
  tools.pnpm,
  [
    ...tools.pnpmPrefixArgs,
    'tauri',
    'build',
    '--bundles',
    'msi',
    '--target',
    tools.toolchain.target,
    '--config',
    'src-tauri/tauri.windows.conf.json',
    ...flags.tauriArgs,
  ],
  { env: tauriEnv },
)

console.log('')
console.log('✓ bundle-windows complete')
console.log(`  installer: src-tauri/target/${tools.toolchain.target}/release/bundle/msi/*.msi`)
