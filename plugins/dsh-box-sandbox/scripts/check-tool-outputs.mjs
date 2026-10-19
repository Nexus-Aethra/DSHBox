#!/usr/bin/env node
/**
 * Run every box_* tool against the live daemon and validate each result against
 * the tool's own output schema, the way the host does.
 *
 * The host only validates a tool's result at the moment it is called, and only
 * with the code it loaded at startup. That makes a schema bug something a
 * person finds by restarting their desktop and pressing the button -- which is
 * exactly the loop this exists to break. Here the plugin's execute() is called
 * directly and the answer is checked the moment it comes back, so a change is
 * proven before anyone is asked to restart anything.
 *
 * The schema rules are DSH's own: seven types, oneOf, properties, required,
 * additionalProperties, items, enum, const. DSH rejects a type array, so
 * nullable is expressed as oneOf -- the same constraint that made five tools
 * fail to register.
 *
 * Usage: node scripts/check-tool-outputs.mjs [--tool box_scroll] [--keep]
 */
import { readFileSync, existsSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { dirname, join, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const pluginDir = resolve(here, '..')
const DIST = join(pluginDir, 'dist', 'index.mjs')
const DISCOVERY = join(process.env.USERPROFILE || 'C:/Users/hp', '.dsh-box', 'server', 'discovery.json')

if (!existsSync(DIST)) {
  console.error('no build; run \'pnpm build\' first: ' + DIST)
  process.exit(2)
}
if (!existsSync(DISCOVERY)) {
  console.error('no discovery record: ' + DISCOVERY)
  console.error('the daemon is not running, so there is nothing to call')
  process.exit(2)
}

const only = process.argv.includes('--tool') ? process.argv[process.argv.indexOf('--tool') + 1] : null
const keep = process.argv.includes('--keep')

const SCHEMA_TYPES = ['object', 'array', 'string', 'number', 'integer', 'boolean', 'null']

function losslessJson(value) {
  if (value === null) return true
  const t = typeof value
  if (t === 'string' || t === 'boolean') return true
  if (t === 'number') return Number.isFinite(value)
  if (t !== 'object') return false
  if (Array.isArray(value)) return value.every(losslessJson)
  const proto = Object.getPrototypeOf(value)
  if (proto !== null && proto !== Object.prototype) return false
  return Object.values(value).every(losslessJson)
}

/** Validate one value against one schema node. Returns a list of complaints. */
function check(schema, value, path) {
  const out = []
  if (!schema || typeof schema !== 'object') return out

  if (Object.hasOwn(schema, 'oneOf')) {
    const branches = schema.oneOf
    const matched = branches.filter((branch) => check(branch, value, path).length === 0).length
    if (matched !== 1) {
      out.push(
        matched === 0
          ? '"' + path + '" matches no oneOf branch'
          : '"' + path + '" matches ' + matched + ' oneOf branches, which is ambiguous',
      )
    }
    return out
  }

  const type = schema.type
  if (type === undefined) {
    if (!losslessJson(value)) out.push('"' + path + '" is not a lossless JSON value')
    return out
  }
  if (!SCHEMA_TYPES.includes(type)) {
    out.push('"' + path + '" declares an unsupported type ' + JSON.stringify(type))
    return out
  }

  const ok =
    type === 'object' ? value !== null && typeof value === 'object' && !Array.isArray(value)
    : type === 'array' ? Array.isArray(value)
    : type === 'string' ? typeof value === 'string'
    : type === 'boolean' ? typeof value === 'boolean'
    : type === 'null' ? value === null
    : type === 'number' ? typeof value === 'number' && Number.isFinite(value)
    : type === 'integer' ? typeof value === 'number' && Number.isInteger(value)
    : false
  if (!ok) {
    out.push('"' + path + '" must be a ' + type + ', got ' + (value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value))
    return out
  }

  if (type === 'object') {
    const properties = Object.hasOwn(schema, 'properties') ? schema.properties ?? {} : {}
    for (const key of Object.hasOwn(schema, 'required') ? schema.required ?? [] : []) {
      if (!Object.hasOwn(value, key) || value[key] === undefined) {
        out.push('missing required property "' + path + '.' + key + '"')
      }
    }
    if (schema.additionalProperties === false) {
      for (const key of Object.keys(value)) {
        if (!Object.hasOwn(properties, key)) {
          out.push('"' + path + '.' + key + '" is not a declared property (additionalProperties: false)')
        }
      }
    }
    for (const [key, child] of Object.entries(properties)) {
      if (Object.hasOwn(value, key) && value[key] !== undefined) {
        out.push(...check(child, value[key], path + '.' + key))
      }
    }
  } else if (type === 'array') {
    if (schema.items) {
      value.forEach((item, index) => out.push(...check(schema.items, item, path + '[' + index + ']')))
    }
  } else if (type === 'string' && schema.enum && !schema.enum.includes(value)) {
    out.push('"' + path + '" is not one of ' + JSON.stringify(schema.enum))
  }

  if (Object.hasOwn(schema, 'const') && value !== schema.const) {
    out.push('"' + path + '" must equal ' + JSON.stringify(schema.const))
  }
  return out
}

// Load the built plugin and capture what it registers. hostServices(ctx) is the
// context itself, so a plain object with tools.register is the whole host
// surface these tools need.
const registered = new Map()
const ctx = {
  tools: { register: (definition) => registered.set(definition.name, definition) },
  effect: (fn) => { ctx.__cleanup = fn; return ctx },
  onDispose: (fn) => { ctx.__dispose = fn; return ctx },
}
const mod = await import(pathToFileURL(DIST).href)
mod.apply(ctx)
ctx.__dispose?.()

const discovery = JSON.parse(readFileSync(DISCOVERY, 'utf8'))
const containers = await fetch('http://127.0.0.1:' + discovery.port + '/rpc', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ token: discovery.token, method: 'list_containers' }),
}).then((response) => response.json())
// list_containers answers a bare array, not an object with a containers key.
const listed = Array.isArray(containers.result) ? containers.result : (containers.result?.containers ?? [])
const running = listed.find((entry) => String(entry.status ?? '').toLowerCase() === 'running') ?? listed[0]
const containerId = running?.id
if (!containerId) {
  console.error('no container to drive; start one first')
  process.exit(2)
}

const ARGS = {
  box_overview: {},
  box_lifecycle: { containerId, action: 'start' },
  box_create: { action: 'describe', containerId },
  box_resources: { action: 'list', containerId },
  box_plugins: { action: 'list' },
  box_templates: { action: 'list' },
  box_settings: { action: 'installed' },
  box_task: { action: 'list' },
  box_workspace: { action: 'list', containerId },
  box_set_browser: {},
  box_page_text: { containerId, limit: 3 },
  box_scroll: { containerId },
  box_type_text: { containerId, text: 'probe' },
  box_console: { containerId },
  box_press_key: { containerId, key: 'Escape' },
  box_click_at: { containerId, x: 5, y: 5 },
  box_click_element: { containerId, name: '设置', role: 'button' },
  box_close: { containerId },
}

// These two cannot be driven from outside the host, and pretending otherwise
// turns a harness limitation into a reported tool failure. box_screenshot
// writes through the Tauri IPC bridge, which does not exist in a bare node
// process. box_click_element needs a control the page currently shows, and a
// previous run may have opened the settings dialog that owns the name.
const SKIP = new Map([
  ['box_screenshot', 'writes through the Tauri IPC bridge, which a bare node process has no way to provide'],
  ['box_click_element', 'needs a control the page is currently showing; probe by hand if you need this one'],
])

// box_close ends the session, so it goes last no matter what order they were
// registered in. Everything after it fails for a reason that is its own.
const names = [...registered.keys()].filter((n) => n in ARGS)
const ordered = [...names.filter((n) => n !== 'box_close'), ...names.filter((n) => n === 'box_close')]

let pass = 0
let fail = 0
let skipped = 0
const failures = []

for (const name of ordered) {
  if (only && name !== only) { skipped++; continue }
  if (SKIP.has(name)) { skipped++; continue }
  let value
  let thrown = null
  try {
    value = await registered.get(name).execute(ARGS[name])
  } catch (error) {
    thrown = String(error?.message ?? error)
  }
  if (thrown) {
    fail++
    failures.push([name, 'threw', thrown])
    console.log('  FAIL  ' + name.padEnd(20) + thrown.slice(0, 110))
    continue
  }
  const schema = registered.get(name).output?.schema
  if (!schema) { skipped++; continue }
  const problems = check(schema, value, 'value')
  if (problems.length) {
    fail++
    failures.push([name, 'invalid', problems])
    for (const problem of problems.slice(0, 3)) console.log('  FAIL  ' + name.padEnd(20) + problem)
  } else {
    pass++
    console.log('  ok    ' + name)
  }
}

if (!keep) {
  try { await registered.get('box_close')?.execute({ containerId }) } catch { /* the session is going away anyway */ }
}

console.log('\n' + pass + ' ok, ' + fail + ' failed, ' + skipped + ' skipped')
for (const [name, why] of SKIP) {
  if (ordered.includes(name)) console.log('  skip  ' + name.padEnd(20) + why)
}
process.exit(fail > 0 ? 1 : 0)
