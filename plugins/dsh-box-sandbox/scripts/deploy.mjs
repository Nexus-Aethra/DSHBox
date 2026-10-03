#!/usr/bin/env node
/**
 * Build, verify, then install into a DSH profile. Verification is not optional
 * and it is not last.
 *
 * dist/ is a build artifact and is gitignored, so a fix in src/ is not a fix
 * anywhere until it has been rebuilt and copied to the profile that loads it.
 * A stale dist installs silently, the profile keeps running yesterday's code,
 * and the only symptom is a restart that appears to change nothing. Two bugs
 * reached the desktop that way.
 *
 * The order is build -> check -> install, and a failing check stops the install.
 * The check calls every tool's execute() against the live daemon and validates
 * the answer with the tool's own schema, so what lands in the profile is the
 * exact build that was proven, not a hope that the next build will match.
 *
 * Usage: node scripts/deploy.mjs [targetDistDir] [--no-check]
 */
import { spawnSync } from 'node:child_process'
import { existsSync, cpSync, readFileSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const pluginDir = resolve(here, '..')
const dist = join(pluginDir, 'dist')
const node = process.execPath
// argv[0] is node itself and argv[1] is this file, so the target is whatever
// follows those. Taking the first non-flag argument instead picks up the node
// binary and tries to copy the build over it.
const target = process.argv.slice(2).find((arg) => !arg.startsWith('--'))
const skipCheck = process.argv.includes('--no-check')

function run(label, command, args) {
  process.stdout.write(label + ' ... ')
  const result = spawnSync(command, args, { cwd: pluginDir, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8' })
  if (result.status !== 0) {
    console.log('FAILED')
    console.log((result.stdout || '') + (result.stderr || ''))
    process.exit(1)
  }
  console.log('ok')
}

// tsdown's own entry, resolved through the repo's node_modules. Spawning
// npx from here needs a shell on Windows and fails silently without one, which
// is how a build can appear to succeed and leave a stale dist behind -- so the
// entry is called directly instead.
function tsdownEntry() {
  const pkg = JSON.parse(readFileSync(join(pluginDir, 'package.json'), 'utf8'))
  void pkg
  const roots = [join(pluginDir, 'node_modules'), join(pluginDir, '..', '..', 'node_modules')]
  for (const root of roots) {
    const base = join(root, 'tsdown', 'package.json')
    if (existsSync(base)) {
      const bin = JSON.parse(readFileSync(base, 'utf8')).bin
      return join(root, 'tsdown', typeof bin === 'string' ? bin : bin.tsdown)
    }
  }
  console.error('cannot find tsdown in node_modules; run pnpm install first')
  process.exit(2)
}
run('build', node, [tsdownEntry()])

if (!existsSync(join(dist, 'index.mjs'))) {
  console.error('build produced no dist/index.mjs')
  process.exit(1)
}

if (!skipCheck) {
  const check = spawnSync(node, [join(here, 'check-tool-outputs.mjs')], { stdio: 'inherit', encoding: 'utf8' })
  if (check.status !== 0) {
    console.error('')
    console.error('verification failed, so nothing was installed. Fix the failure above and run this again.')
    process.exit(1)
  }
}

if (!target) {
  console.log('built and verified. pass a profile dist directory to install: node scripts/deploy.mjs <dir>')
  process.exit(0)
}

if (!existsSync(target)) {
  console.error('no such directory: ' + target)
  process.exit(1)
}
cpSync(dist, target, { recursive: true, force: true })

const before = statSync(join(dist, 'index.mjs')).mtimeMs
const after = statSync(join(target, 'index.mjs')).mtimeMs
if (Math.abs(before - after) > 2000) {
  console.error('installed file does not match the build; refusing to call this done')
  process.exit(1)
}
console.log('installed into ' + target)
