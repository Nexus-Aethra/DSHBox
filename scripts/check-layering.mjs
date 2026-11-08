#!/usr/bin/env node
// Fails when a crate reaches across a layer boundary.
//
// The four directories are only documentation until something checks them: a
// misplaced `path` dependency compiles fine and quietly turns the layout into
// the flat list it replaced. Rules:
//
//   core   -> core only            (no business knowledge, ever)
//   domain -> core + domain        (business capabilities, a DAG among themselves)
//   edge   -> core + domain + edge (wire types, transport, client adapter)
//   apps   -> anything, but not another app, and never the Tauri package
//
// Two extra rules that are not layer-shaped: `box-state` may not depend on
// `box-template-core` (that pair is what makes the data-scheduler merge acyclic),
// and no crate may depend on `dshbox` (only src-tauri itself may).
import { existsSync, readdirSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'

const CRATES = join('src-tauri', 'crates')
const LAYERS = ['core', 'domain', 'edge', 'apps']
const ALLOWED = {
  core: ['core'],
  domain: ['core', 'domain'],
  edge: ['core', 'domain', 'edge'],
  apps: ['core', 'domain', 'edge', 'apps'],
}

function cratesIn(layer) {
  const dir = join(CRATES, layer)
  if (!existsSync(dir)) return []
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(dir, entry.name, 'Cargo.toml')))
    .map((entry) => ({ name: entry.name, layer, dir: join(dir, entry.name) }))
}

const crates = LAYERS.flatMap(cratesIn)
const byDir = new Map(crates.map((crate) => [resolve(crate.dir), crate]))
const violations = []

for (const crate of crates) {
  const manifest = join(crate.dir, 'Cargo.toml')
  const text = readFileSync(manifest, 'utf8')
  const re = /path\s*=\s*"([^"]+)"/g
  let match
  while ((match = re.exec(text)) !== null) {
    const target = byDir.get(resolve(dirname(manifest), match[1]))
    if (!target) continue
    if (!ALLOWED[crate.layer].includes(target.layer)) {
      violations.push(`${crate.layer}/${crate.name} depends on ${target.layer}/${target.name}`)
      continue
    }
    if (crate.layer === 'apps' && target.layer === 'apps' && crate.name !== target.name) {
      violations.push(`apps/${crate.name} depends on another app (apps/${target.name})`)
    }
    if (crate.name === 'box-state' && target.name === 'box-template-core') {
      violations.push('domain/box-state depends on domain/box-template-core (closes the merge cycle)')
    }
  }
  if (/\bdshbox\s*=/.test(text) && crate.name !== 'dshbox') {
    violations.push(`${crate.layer}/${crate.name} depends on the Tauri package dshbox`)
  }
}

// The Tauri package itself may use any layer, but never another app.
const rootManifest = join('src-tauri', 'Cargo.toml')
if (existsSync(rootManifest)) {
  const text = readFileSync(rootManifest, 'utf8')
  if (/path\s*=\s*"\.\/crates\/apps\//.test(text)) {
    violations.push('the Tauri package depends on a binary crate under crates/apps')
  }
}

if (violations.length > 0) {
  console.error(`layering: ${violations.length} violation(s)`)
  for (const violation of violations) console.error(`  - ${violation}`)
  console.error('layout: docs/design/2026-11-08-node-link-session-and-crate-layering.md section 14')
  process.exit(1)
}
console.log(`layering ok: ${crates.length} crates across ${LAYERS.join(' / ')}`)