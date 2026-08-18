import { existsSync, mkdirSync, cpSync, rmSync, readdirSync, statSync, readFileSync, createWriteStream } from 'node:fs'
import { createHash } from 'node:crypto'
import { dirname, join, relative } from 'node:path'
import { spawnSync } from 'node:child_process'
import { isFresh } from './mtime.mjs'

// Resolve the target triple, mirroring scripts/prepare-runtime.mjs.
const target = process.argv.includes('--target')
  ? process.argv[process.argv.indexOf('--target') + 1]
  : process.env.DSH_BOX_RUNTIME_TARGET
      ?? `${process.platform === 'win32' ? 'win' : process.platform === 'darwin' ? 'macos' : 'linux'}-${process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : process.arch}`

const root = process.cwd()
const pluginSource = join(root, 'src-tauri/crates/box-dsh-context/dsh-box-context')
const pluginPackage = '@deepseek-ai/dsh-box-context'
const output = join(root, 'src-tauri/resources/plugins', target)

if (!existsSync(pluginSource)) {
  console.error(`plugin source missing: ${pluginSource}`)
  process.exit(1)
}

const force = process.argv.includes('--force')

// Skip rebuilding while the vendored plugin tree is newer than every
// tracked source file under the plugin package. Pass --force to rebuild.
const sources = [join(root, 'src-tauri/crates/box-dsh-context/dsh-box-context')]
if (isFresh(output, sources, force)) {
  console.log(`plugins already prepared at ${output}; pass --force to rebuild`)
  process.exit(0)
}

// 1. Build the TypeScript plugin into dist/.
const build = spawnSync('pnpm', ['--dir', pluginSource, 'build'], { stdio: 'inherit' })
if (build.status !== 0) process.exit(build.status ?? 1)

// 2. Clean the destination and copy the package skeleton.
rmSync(output, { recursive: true, force: true })
mkdirSync(output, { recursive: true })
const pkgDir = join(output, 'node_modules', pluginPackage)
mkdirSync(pkgDir, { recursive: true })

for (const entry of ['dist', 'cordis.patch.yml', 'README.md', 'package.json']) {
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
  pluginVersion: '0.1.0',
  builtAt: new Date().toISOString(),
  files: {},
}

for (const file of walk(pkgDir)) {
  manifest.files[relative(pkgDir, file).replaceAll('\\', '/')] = hashFile(file)
}

const manifestPath = join(output, 'plugins-manifest.json')
createWriteStream(manifestPath).end(JSON.stringify(manifest, null, 2))
console.log(`wrote ${relative(root, manifestPath)} (${Object.keys(manifest.files).length} files)`)
