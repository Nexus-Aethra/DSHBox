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
//   apps   -> anything, but not another app and never the Tauri package
//   root   -> core + domain + edge, but not a binary under apps/
//
// Two rules that are not layer-shaped, checked the same way: `box-state` may not
// depend on `box-template-core` (that pair is what keeps the data-scheduler merge
// acyclic), and no crate may depend on the `dshbox` package (only src-tauri may).
//
// Both dependency spellings are resolved: `path = "..."` and `workspace = true`
// (which points at the root's [workspace.dependencies] table). Missing the second
// one made this guard report green on a core -> domain violation.
import { existsSync, readdirSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'

const ROOT_MANIFEST = join('src-tauri', 'Cargo.toml')
const CRATES = join('src-tauri', 'crates')
const LAYERS = ['core', 'domain', 'edge', 'apps']
const ALLOWED = {
  core: ['core'],
  domain: ['core', 'domain'],
  edge: ['core', 'domain', 'edge'],
  apps: ['core', 'domain', 'edge', 'apps'],
  root: ['core', 'domain', 'edge'],
}

function cratesIn(layer) {
  const dir = join(CRATES, layer)
  if (!existsSync(dir)) return []
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(dir, entry.name, 'Cargo.toml')))
    .map((entry) => ({ name: entry.name, layer, dir: join(dir, entry.name) }))
}

// Dependencies declared once in the root and pulled in with `workspace = true`.
function workspaceDependencies() {
  const table = new Map()
  if (!existsSync(ROOT_MANIFEST)) return table
  const text = readFileSync(ROOT_MANIFEST, 'utf8')
  const section = text.split(/^\[workspace\.dependencies\]$/m)[1]?.split(/^\[/m)[0]
  if (!section) return table
  const re = /^\s*([\w-]+)\s*=\s*\{[^}]*path\s*=\s*"([^"]+)"/gm
  let match
  while ((match = re.exec(section)) !== null) {
    table.set(match[1], resolve('src-tauri', match[2]))
  }
  return table
}

function declaredDependencies(manifest, workspace) {
  const text = readFileSync(manifest, 'utf8')
  const dir = dirname(manifest)
  const deps = []
  const pathRe = /path\s*=\s*"([^"]+)"/g
  let match
  while ((match = pathRe.exec(text)) !== null) {
    deps.push(resolve(dir, match[1]))
  }
  // name = { workspace = true }
  const inlineRe = /^\s*([\w-]+)\s*=\s*\{[^}]*workspace\s*=\s*true/gm
  while ((match = inlineRe.exec(text)) !== null) {
    const target = workspace.get(match[1])
    if (target) deps.push(target)
  }
  // name.workspace = true
  const dottedRe = /^\s*([\w-]+)\.workspace\s*=\s*true/gm
  while ((match = dottedRe.exec(text)) !== null) {
    const target = workspace.get(match[1])
    if (target) deps.push(target)
  }
  return deps
}

const workspace = workspaceDependencies()
const crates = LAYERS.flatMap(cratesIn)
const root = { name: 'dshbox', layer: 'root', dir: 'src-tauri' }
const byDir = new Map([...crates, root].map((node) => [resolve(node.dir), node]))
const violations = []

for (const crate of crates) {
  for (const target of declaredDependencies(join(crate.dir, 'Cargo.toml'), workspace)) {
    const node = byDir.get(target)
    if (!node) continue
    if (node.layer === 'root') {
      violations.push(`${crate.layer}/${crate.name} depends on the Tauri package (${node.name})`)
      continue
    }
    if (!ALLOWED[crate.layer].includes(node.layer)) {
      violations.push(`${crate.layer}/${crate.name} depends on ${node.layer}/${node.name}`)
      continue
    }
    if (crate.layer === 'apps' && node.layer === 'apps' && crate.name !== node.name) {
      violations.push(`apps/${crate.name} depends on another app (apps/${node.name})`)
    }
    if (crate.name === 'box-state' && node.name === 'box-template-core') {
      violations.push('domain/box-state depends on domain/box-template-core (closes the merge cycle)')
    }
  }
}

// The Tauri package may use any layer, but never a binary under apps/.
if (existsSync(ROOT_MANIFEST)) {
  for (const target of declaredDependencies(ROOT_MANIFEST, workspace)) {
    const node = byDir.get(target)
    if (node && node.layer === 'apps') {
      violations.push(`the Tauri package depends on the binary ${node.layer}/${node.name}`)
    }
  }
}

if (violations.length > 0) {
  console.error(`layering: ${violations.length} violation(s)`)
  for (const violation of violations) console.error(`  - ${violation}`)
  console.error('layout: docs/design/2026-11-08-node-link-session-and-crate-layering.md section 14')
  process.exit(1)
}
console.log(`layering ok: ${crates.length} crates across ${LAYERS.join(' / ')}`)