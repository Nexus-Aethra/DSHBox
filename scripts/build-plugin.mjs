import { existsSync, mkdirSync, cpSync, rmSync, readdirSync, statSync, readFileSync, writeFileSync, createWriteStream } from 'node:fs'
import { createHash } from 'node:crypto'
import { delimiter, dirname, join, relative } from 'node:path'
import { spawnSync } from 'node:child_process'
import { hashInputs, isFresh } from './mtime.mjs'

// Bundle the sandbox plugin (the one the agent inside a container drives the
// box through) into `resources/plugins/<target>/`, which Tauri ships with the
// app and the daemon vendors into `<runtime>/plugins/` at startup.
//
// This used to bundle `@deepseek-ai/dsh-box-context`, a plugin that rendered a
// container-metadata prompt section. It never loaded in a prepared/sealed
// container (`Cannot find package '@deepseek-ai/dsh-box-context'` in every
// host.log) and its one real job -- telling the agent which paths it owns --
// is better carried by the plugin the box actually wants mounted.

// Resolve the target triple, mirroring scripts/prepare-runtime.mjs.
const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET
      ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

const root = process.cwd()
// The plugin source is the workspace package, not a vendored copy: it is the
// same tree `pnpm build` and `check:outputs` run against.
const pluginSource = join(root, 'plugins', 'dsh-box-sandbox')
const pluginPackage = '@nexus-aethra/dsh-box-sandbox'
const output = join(root, 'src-tauri/resources/plugins', target)

if (!existsSync(pluginSource)) {
  console.error(`plugin source missing: ${pluginSource}`)
  process.exit(1)
}

const force = process.argv.includes('--force')

// Skip rebuilding while the built tree is newer than every tracked source
// file of the plugin (src/, plus the manifest and patch that ship with it).
const sources = [
  join(pluginSource, 'src'),
  join(pluginSource, 'package.json'),
  join(pluginSource, 'cordis.patch.yml'),
  join(pluginSource, 'skills'),
]
if (isFresh(output, sources, force)) {
  console.log(`plugins already prepared at ${output}; pass --force to rebuild`)
  process.exit(0)
}

// Stronger hash-cache: hash the plugin's package.json + the lockfile so the
// plugin build only runs when its declared dependencies or version actually
// changed. Without this, every workspace mtime drift (Cargo.lock, sibling
// crates) wastes a full pnpm install + build.
const cacheRoot = join(root, '.cache', 'dsh-box', 'plugin')
mkdirSync(cacheRoot, { recursive: true })
const cacheFile = join(cacheRoot, `${target}.sha256`)
const hashInputs_ = [
  join(pluginSource, 'package.json'),
  join(root, 'pnpm-lock.yaml'),
  join(pluginSource, 'tsconfig.json'),
]
if (hashInputs(cacheFile, hashInputs_, force)) {
  if (existsSync(join(output, 'plugins-manifest.json'))) {
    console.log(`plugin inputs unchanged (hash cache hit at ${cacheFile}); skipping pnpm build`)
    process.exit(0)
  }
}

// 1. Build the TypeScript plugin into dist/.
//    On Windows, `pnpm` is typically pnpm.cmd. Bare spawnSync('pnpm')
//    without shell:true hits EINVAL. Walk PATH + well-known dirs to find
//    pnpm (.cmd/.exe) and prefer corepack as a fallback (it ships with
//    every modern Node install and can dispatch to pnpm directly).
function findCommand(cmd, extraDirs = []) {
  const names = process.platform === 'win32'
    ? [`${cmd}.cmd`, `${cmd}.bat`, `${cmd}.exe`, cmd]
    : [cmd]
  const dirs = (process.env.PATH || '').split(delimiter).filter(Boolean)
  for (const dir of [...dirs, ...extraDirs]) {
    for (const name of names) {
      const candidate = join(dir, name)
      // A directory is not a command. Box's install directory sits on PATH and
      // holds a `pnpm/` data directory, so existsSync alone picks that and
      // spawnSync then fails with ENOENT on something that was never a file.
      try {
        if (statSync(candidate).isFile()) return candidate
      } catch {
        // Missing or unreadable: not a candidate.
      }
    }
  }
  return null
}

function wellKnownBin() {
  const sysdrive = process.env.SystemDrive || 'C:'
  const programFiles = process.env['ProgramFiles'] || join(sysdrive, 'Program Files')
  const userprofile = process.env.USERPROFILE || ''
  return [
    join(sysdrive, 'nodejs'),
    join(sysdrive, 'nodejs', 'node_global'),
    join(programFiles, 'nodejs'),
    join(userprofile, '.cargo', 'bin'),
  ]
}

function resolvePnpmInvocation() {
  const pnpmArgs = ['--dir', pluginSource, 'build']
  if (process.platform !== 'win32') {
    return { cmd: 'pnpm', args: pnpmArgs, shell: false }
  }
  const direct = findCommand('pnpm', wellKnownBin())
  if (direct) {
    const lower = direct.toLowerCase()
    const shell = lower.endsWith('.cmd') || lower.endsWith('.bat')
    return { cmd: direct, args: pnpmArgs, shell }
  }
  const corepack = findCommand('corepack', wellKnownBin())
  if (corepack) {
    return { cmd: corepack, args: ['pnpm', ...pnpmArgs], shell: true }
  }
  // Last resort: assume pnpm is on PATH through the user's shell init.
  return { cmd: 'pnpm', args: pnpmArgs, shell: true }
}

const pnpm = resolvePnpmInvocation()
const build = spawnSync(pnpm.cmd, pnpm.args, { stdio: 'inherit', shell: pnpm.shell })
if (build.status !== 0) {
  // Say WHY. A bundler invoked through a shell that cannot find it exits 1
  // with nothing on either stream, and "the plugin did not build" with no
  // message send the reader looking for a TypeScript error that never existed.
  if (build.error) console.error(`cannot run ${pnpm.cmd}: ${build.error.message}`)
  console.error(
    `the plugin build failed (exit ${build.status ?? 'signal ' + build.signal}); ` +
    `run "pnpm --dir ${pluginSource} build" to see the compiler output`,
  )
  process.exit(build.status ?? 1)
}

// 2. Clean the destination and copy the package skeleton. `skills/` travels
//    with the package because the container-start path installs them into the
//    container's DSH skill root.
rmSync(output, { recursive: true, force: true })
mkdirSync(output, { recursive: true })
const pkgDir = join(output, 'node_modules', pluginPackage)
mkdirSync(pkgDir, { recursive: true })

for (const entry of ['dist', 'cordis.patch.yml', 'README.md', 'package.json', 'skills']) {
  if (!existsSync(join(pluginSource, entry))) {
    if (entry === 'skills') continue
    throw new Error(`plugin source is missing ${entry}`)
  }
  cpSync(join(pluginSource, entry), join(pkgDir, entry), { recursive: true })
}

// 3. Walk dependencies recursively from the plugin's package.json.
//    pnpm stores each package as node_modules/.pnpm/<escaped-name>@<version>/node_modules/<name>
//    where every '/' in the original name becomes '+' (leading '@' preserved).
//    We resolve each dep through that store, copy its directory into
//    pkgDir/node_modules/<name>, then recurse into the dep's own deps.
const pnpmRoot = join(root, 'node_modules/.pnpm')
const vendored = new Set()

function pnpmEscape(name) { return name.replace(/\//g, '+') }

function findInPnpmStore(dep) {
  if (!existsSync(pnpmRoot)) return null
  const prefix = pnpmEscape(dep) + '@'
  for (const entry of readdirSync(pnpmRoot)) {
    if (!entry.startsWith(prefix)) continue
    const candidate = join(pnpmRoot, entry, 'node_modules', dep)
    if (existsSync(join(candidate, 'package.json'))) return candidate
  }
  return null
}

function vendorDependency(dep, under) {
  const key = `${under}::${dep}`
  if (vendored.has(key)) return
  vendored.add(key)

  const source = findInPnpmStore(dep)
  if (source === null) throw new Error(`cannot resolve ${dep} for vendor under ${under}`)

  const targetDir = join(under, dep)
  mkdirSync(dirname(targetDir), { recursive: true })
  cpSync(source, targetDir, { recursive: true })
  console.log(`vendored ${dep} -> ${relative(root, targetDir)}`)

  // Recurse into this dep's own deps.
  const depPkg = JSON.parse(readFileSync(join(targetDir, 'package.json'), 'utf8'))
  for (const subDep of Object.keys(depPkg.dependencies ?? {})) {
    vendorDependency(subDep, targetDir + '/node_modules')
  }
}

const pluginPkgJson = JSON.parse(readFileSync(join(pkgDir, 'package.json'), 'utf8'))
for (const dep of Object.keys(pluginPkgJson.dependencies ?? {})) {
  vendorDependency(dep, join(pkgDir, 'node_modules'))
}

// 4. Walk the vendored tree and hash every regular file into plugins-manifest.json.
function hashFile(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

function walk(dir) {
  const out = []
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry)
    const st = statSync(p)
    if (st.isDirectory()) out.push(...walk(p))
    else if (st.isFile()) out.push(p)
  }
  return out
}

const manifest = {
  target,
  pluginPackage,
  pluginVersion: pluginPkgJson.version,
  builtAt: new Date().toISOString(),
  files: {},
}

for (const file of walk(pkgDir)) {
  manifest.files[relative(pkgDir, file).replaceAll('\\', '/')] = hashFile(file)
}

const manifestPath = join(output, 'plugins-manifest.json')
writeFileSync(manifestPath, JSON.stringify(manifest, null, 2))
console.log(`wrote ${relative(root, manifestPath)} (${Object.keys(manifest.files).length} files)`)
