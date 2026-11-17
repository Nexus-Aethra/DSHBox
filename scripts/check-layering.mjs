#!/usr/bin/env node
// Fails when a crate reaches across a layer boundary.
//
// The four directories are only documentation until something checks them: a
// misplaced dependency compiles fine and quietly turns the layout into the flat
// list it replaced. Rules:
//
//   core   -> core only            (no business knowledge, ever)
//   domain -> core + domain        (business capabilities, a DAG among themselves)
//   edge   -> core + domain + edge (wire types, transport, client adapter)
//   apps   -> core + domain + edge + apps, but not another app (and not tools/)
//   tools  -> core + domain + edge (a build tool never links a binary)
//   root   -> core + domain + edge (the Tauri package never links a binary)
//
// Two rules that are not layer-shaped, checked the same way: `box-state` may not
// depend on `box-template-core` (that pair is what keeps the data-scheduler merge
// acyclic), and no crate may depend on the `dshbox` package (only src-tauri may).
//
// The graph comes from `cargo metadata`, not from a regex over the manifests.
// A text scan cannot tell a dependency from a comment, misses
// `[dependencies.foo]` tables and `workspace = true` inheritance, and sees only
// the crates it is told about -- each of which made this guard report green on a
// real violation (review of PR #40). Cargo resolves the manifests, so every
// spelling is covered by construction, and `--no-deps` keeps it offline (it reads
// this workspace's manifests, nothing else). The member list comes from the same
// place, so a crate outside the four layer directories is a violation rather
// than something the scan never saw.
import { execFileSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

// Anchored to this file, not to the caller's working directory: run from
// `src-tauri`, a relative lookup found no workspace members and printed
// "layering ok: 0 crates" with exit 0 -- a green check that checked nothing.
const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const WORKSPACE_MANIFEST = join(REPO_ROOT, 'src-tauri', 'Cargo.toml')
const LAYERS = ['core', 'domain', 'edge', 'apps']
const ALLOWED = {
  core: ['core'],
  domain: ['core', 'domain'],
  edge: ['core', 'domain', 'edge'],
  apps: ['core', 'domain', 'edge', 'apps'],
  tools: ['core', 'domain', 'edge'],
  root: ['core', 'domain', 'edge'],
}

// The one rule that is not about direction but about the framework: a box
// crate must never link Tauri. Only the root package (the desktop shell) may,
// and `tauri-build` only on the root, where its build script is. A domain/ crate
// that reaches for Tauri drags the whole webview stack into the daemon and the
// node helper -- both of which must stay headless binaries -- so it is checked
// here rather than left to a reader of AGENTS.md.
//
// The family is matched by NAME, not by provenance: cargo metadata carries no
// information about who published a crate, so the real Tauri and an unrelated
// crate called `tauri-utils` are indistinguishable here. Rather than guess, the
// rule covers exactly the names a feature would plausibly reach for -- `tauri`
// itself, any `tauri-plugin-*`, and `tauri-build` -- and leaves everything else
// to the layer loop. That is a deliberate narrowing: a registry dep named
// `tauri-runtime`, `tauri-codegen` or `wry` is NOT caught, even though it can pull
// the webview in transitively. The three covered names are where the webview
// actually enters a build, and a wider match was tried first and removed because
// it refused crates that had nothing to do with Tauri (review of this PR).
//
// `tauri-build` is spelled out separately because it is the one tauri-family
// name that is not a plugin, and it must be allowed on the root (its build
// script) while staying forbidden elsewhere. The root may use exactly the three
// names in ROOT_TAURI_OK; adding a fourth there is a deliberate edit to that
// set, which is what makes the AGENTS.md sentence enforceable rather than
// aspirational -- before this, the root could add any tauri-plugin-* unopposed.
const TAURI_FAMILY = /^tauri(-plugin-|$)/
const TAURI_BUILD = 'tauri-build'
const ROOT_TAURI_OK = new Set(['tauri', 'tauri-plugin-dialog', 'tauri-build'])

function metadata() {
  if (!existsSync(WORKSPACE_MANIFEST)) {
    console.error(`layering: no workspace manifest at ${WORKSPACE_MANIFEST}`)
    process.exit(1)
  }
  let raw
  try {
    raw = execFileSync(
      'cargo',
      ['metadata', '--no-deps', '--offline', '--format-version', '1', '--manifest-path', WORKSPACE_MANIFEST],
      { cwd: REPO_ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 * 1024 * 1024 },
    )
  } catch (error) {
    // Never fall through to a success line on the failure path: "could not read
    // the graph" and "the graph is fine" must not print the same thing.
    const detail = error.stderr?.trim() || error.message
    console.error('layering: cannot read the dependency graph with `cargo metadata`')
    console.error(detail.split('\n').slice(0, 12).map((line) => `  ${line}`).join('\n'))
    console.error('layering: this check requires cargo (the build does too); it has no result without it')
    process.exit(1)
  }
  return JSON.parse(raw)
}

// Which layer a manifest belongs to, from its own path.
//
// `tools/` is a real directory of build tools that may use core/domain/edge but
// never link a binary. Anything else that is not one of the four layers and not
// the Tauri package is an error, whether or not it also has a crossing edge:
// a member outside the layout is exactly the case where the layer rule stops
// meaning anything, and reporting it only when it happens to have an internal
// path dependency made the outcome depend on an unrelated detail.
function classify(manifestPath) {
  const path = manifestPath.split(sep).join('/')
  const cratesPrefix = `${join(REPO_ROOT, 'src-tauri', 'crates').split(sep).join('/')}/`
  if (path === join(REPO_ROOT, 'src-tauri', 'Cargo.toml').split(sep).join('/')) return 'root'
  if (path.startsWith(cratesPrefix)) {
    const [layer] = path.slice(cratesPrefix.length).split('/')
    if (LAYERS.includes(layer)) return layer
    // A crate filed directly under crates/ is outside the layout.
    return 'unlayered'
  }
  if (path.startsWith(`${join(REPO_ROOT, 'src-tauri', 'tools').split(sep).join('/')}/`)) return 'tools'
  return 'unlayered'
}

const workspace = metadata()
const nodes = new Map()
const violations = []
for (const pkg of workspace.packages) {
  const dir = pkg.manifest_path.replace(`${sep}Cargo.toml`, '').split(sep).join('/')
  const layer = classify(pkg.manifest_path)
  nodes.set(dir, { name: pkg.name, layer, dir, manifest: pkg.manifest_path })
  if (layer === 'unlayered') {
    violations.push(
      `${pkg.name} is a workspace member outside core/ domain/ edge/ apps/ (and not a build tool under tools/): ${relative(REPO_ROOT, pkg.manifest_path)}`,
    )
  }
}

for (const pkg of workspace.packages) {
  const self = nodes.get(pkg.manifest_path.replace(`${sep}Cargo.toml`, '').split(sep).join('/'))
  if (!self) continue
  for (const dep of pkg.dependencies) {
    // A registry dependency has no `path`; only a path edge can point into this
    // workspace, and only those can cross a layer.
    if (dep.path === undefined) continue
    const target = nodes.get(dep.path.split(sep).join('/'))
    const kind = dep.kind ?? 'normal'
    if (!target) {
      violations.push(`${self.layer}/${self.name} has a path dependency ${dep.name} (${kind}) that is not a workspace member`)
      continue
    }
    if (target.layer === 'root') {
      violations.push(`${self.layer}/${self.name} depends on the Tauri package (${kind})`)
      continue
    }
    const allowed = ALLOWED[self.layer] ?? []
    if (!allowed.includes(target.layer)) {
      const where = kind === 'normal' ? '' : ` (${kind})`
      violations.push(`${self.layer}/${self.name} depends on ${target.layer}/${target.name}${where}`)
      continue
    }
    if (self.layer === 'apps' && target.layer === 'apps' && self.name !== target.name) {
      violations.push(`apps/${self.name} depends on another app (apps/${target.name})`)
    }
    if (self.name === 'box-state' && target.name === 'box-template-core') {
      violations.push('domain/box-state depends on domain/box-template-core (closes the merge cycle)')
    }
    if (self.layer === 'root' && target.layer === 'apps') {
      violations.push(`the Tauri package depends on the binary ${target.layer}/${target.name}`)
    }
  }
}

// The framework rule. This loop reads registry dependencies (dep.path is
// undefined), which the layer loop above deliberately skips: a registry dep
// cannot point inside this workspace, so it cannot cross a layer -- but it can
// still be the wrong dependency for that crate to have. That was a documented
// gap: AGENTS.md said "feature crates must not depend on Tauri" and nothing
// checked it, so a new domain/ crate could have linked Tauri and gone green.
for (const pkg of workspace.packages) {
  const self = nodes.get(pkg.manifest_path.replace(`${sep}Cargo.toml`, '').split(sep).join('/'))
  if (!self) continue
  const isRoot = self.layer === 'root'
  for (const dep of pkg.dependencies) {
    // A path dependency named tauri-* is a vendored fork, not the framework:
    // every path edge already belongs to the layer loop, so this rule stays out
    // of it. Without this skip a crate could name a local directory `tauri` and
    // be refused for something that is not Tauri at all.
    if (dep.path !== undefined) continue
    if (!TAURI_FAMILY.test(dep.name) && dep.name !== TAURI_BUILD) continue
    if (isRoot && ROOT_TAURI_OK.has(dep.name)) continue
    const where = dep.kind ?? 'normal'
    const holder = isRoot ? 'the Tauri package' : `${self.layer}/${self.name}`
    violations.push(
      isRoot
        ? `the Tauri package depends on ${dep.name} (${where}); the root may only use ${[...ROOT_TAURI_OK].join(', ')}`
        : `${holder} depends on ${dep.name} (${where}); only the dshbox package may link Tauri`,
    )
  }
}

if (violations.length > 0) {
  console.error(`layering: ${violations.length} violation(s)`)
  for (const violation of new Set(violations)) console.error(`  - ${violation}`)
  console.error('layout: docs/design/2026-11-08-node-link-session-and-crate-layering.md section 14')
  process.exit(1)
}
const counted = [...new Set([...nodes.values()].map((node) => node.layer))].filter((layer) => layer !== 'root')
console.log(`layering ok: ${nodes.size} workspace members across ${counted.sort().join(' / ')}`)
