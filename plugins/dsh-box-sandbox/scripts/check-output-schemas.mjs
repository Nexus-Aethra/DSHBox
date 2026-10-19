#!/usr/bin/env node
/**
 * Reconcile every box_* tool output schema against the keys the daemon
 * actually returns.
 *
 * A tool whose schema disagrees with its own result does not work, and the
 * failure is invisible until the host refuses the call: nothing here throws at
 * build time and nothing looks wrong in review. Five of these shipped that way.
 * The daemon is the authority on what it returns, so its json! keys are read
 * and diffed against the schema the plugin declares.
 *
 * This catches shape drift only. It cannot run the tools, so a field that is
 * declared but never populated still passes -- for that, call the tool.
 *
 * Usage: node scripts/check-output-schemas.mjs [--verbose]
 */
import { readFileSync, existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const repo = resolve(here, '..', '..', '..')
const PLUGIN = join(repo, 'plugins', 'dsh-box-sandbox', 'src', 'tools.ts')
const DEBUG_RS = join(repo, 'src-tauri', 'crates', 'dshboxd', 'src', 'debug.rs')

for (const file of [PLUGIN, DEBUG_RS]) {
  if (!existsSync(file)) {
    console.error('not found: ' + file)
    process.exit(2)
  }
}

const plugin = readFileSync(PLUGIN, 'utf8')
const rust = readFileSync(DEBUG_RS, 'utf8')

/**
 * The top-level keys of every `Ok(json!({...}))` in one Rust function.
 *
 * Only the return is read, never the request. A daemon call looks like
 * session.call("Runtime.evaluate", json!({ expression, returnByValue })) followed
 * by Ok(json!({ inserted, focused })), and every key in the function body would
 * put request options and nested entry fields in a set that is supposed to be
 * the top level of the answer.
 */
function daemonKeys(fn) {
  const at = rust.search(new RegExp('fn\\s+' + fn + '\\b'))
  if (at < 0) return null
  const stop = rust.indexOf('\npub(crate) fn ', at + 10)
  const body = rust.slice(at, stop > 0 ? stop : at + 4000)
  const keys = new Set()
  for (const m of body.matchAll(/Ok\(json!\(\{/g)) {
    const open = m.index + m[0].length - 1
    let depth = 0
    for (let i = open; i < body.length; i++) {
      const c = body[i]
      if (c === '{') { depth++; continue }
      if (c === '}') { depth--; if (!depth) { collectTopLevel(body.slice(open, i + 1), keys); break } }
      if (depth === 1 && /^"[A-Za-z_][A-Za-z0-9_]*":/.test(body.slice(i, i + 60))) {
        keys.add(/^"([A-Za-z_][A-Za-z0-9_]*)":/.exec(body.slice(i, i + 60))[1])
      }
    }
  }
  // Some returns are assembled by a helper: click_at_rpc answers
  // Ok(click_response(json!({x, y}), hit)) and every field a caller cares
  // about is set inside click_response as response["landed"] = ... Reading
  // only the Ok literal sees x and y and misses the rest, which then reads as
  // a required field nothing returns.
  for (const m of body.matchAll(/Ok\((\w+)\(/g)) {
    for (const key of keysOfHelper(m[1])) keys.add(key)
  }
  return keys
}

/** Assignment targets inside a helper that builds the answer, like response["x"] = ... */
function keysOfHelper(fn) {
  const at = rust.search(new RegExp('fn\\s+' + fn + '\\b'))
  if (at < 0) return []
  const stop = rust.indexOf('\nfn ', at + 10)
  const body = rust.slice(at, stop > 0 ? stop : at + 1200)
  return [...body.matchAll(/\["([A-Za-z_][A-Za-z0-9_]*)"\]\s*=/g)].map((m) => m[1])
}

/** Top-level keys of one brace-balanced object literal. */
function collectTopLevel(text, into) {
  let depth = 0
  for (let i = 0; i < text.length; i++) {
    const c = text[i]
    if (c === '{') { depth++; if (depth === 1) continue }
    if (c === '}') { depth--; if (!depth) break }
    if (depth !== 1) continue
    if (c !== '"') continue
    const m = /^"([A-Za-z_][A-Za-z0-9_]*)":/.exec(text.slice(i, i + 60))
    if (m) into.add(m[1])
  }
}

/** The property names one tool declares in its output schema. */
function declaredProperties(toolName) {
  const at = plugin.indexOf("name: '" + toolName + "'")
  if (at < 0) return null
  const seg = plugin.slice(at, at + 3000)
  const out = seg.indexOf('output:')
  if (out < 0) return null
  const si = seg.indexOf('schema:', out)
  if (si < 0) return null
  const open = seg.indexOf('{', si)
  let depth = 0
  let close = open
  for (let i = open; i < seg.length; i++) {
    if (seg[i] === '{') depth++
    else if (seg[i] === '}') { depth--; if (!depth) { close = i; break } }
  }
  const block = seg.slice(si, close)
  const pi = block.indexOf('properties:')
  if (pi < 0) return null
  const po = block.indexOf('{', pi)
  depth = 0
  let pc = po
  for (let i = po; i < block.length; i++) {
    if (block[i] === '{') depth++
    else if (block[i] === '}') { depth--; if (!depth) { pc = i; break } }
  }
  const props = new Set()
  for (const m of block.slice(po, pc).matchAll(/^\s+([A-Za-z_][A-Za-z0-9_]*):/gm)) props.add(m[1])
  const ri = block.indexOf('required:')
  const required = new Set()
  if (ri >= 0) {
    const ro = block.indexOf('[', ri)
    const rc = block.indexOf(']', ro)
    for (const m of block.slice(ro, rc).matchAll(/'([A-Za-z_][A-Za-z0-9_]*)'/g)) required.add(m[1])
  }
  return { props, required }
}

/** Where a tool's definition starts in the plugin source. */
function at0(toolName) {
  return plugin.indexOf("name: '" + toolName + "'")
}

/** Which daemon method each tool calls, read from its own execute body. */
function methodOf(toolName) {
  const at = at0(toolName)
  if (at < 0) return null
  const seg = plugin.slice(at, at + 4000)
  const ei = seg.indexOf('async execute')
  if (ei < 0) return null
  const body = seg.slice(ei, ei + 900)
  const m = body.match(/['"]debug_[a-z_]+['"]/)
  return m ? m[0].slice(1, -1) : null
}

const METHOD_TO_RPC = {
  debug_page_text: 'page_text_rpc',
  debug_screenshot: 'screenshot_rpc',
  debug_click_element: 'click_element_rpc',
  debug_click_at: 'click_at_rpc',
  debug_type_text: 'type_text_rpc',
  debug_press_key: 'press_key_rpc',
  debug_scroll: 'scroll_rpc',
  debug_set_viewport: 'set_viewport_rpc',
  debug_close: 'close_rpc',
  debug_console: 'console_rpc',
}

const verbose = process.argv.includes('--verbose')
let checked = 0
let bad = 0

for (const m of plugin.matchAll(/name: '(box_[a-z_]+)'/g)) {
  const tool = m[1]
  const method = methodOf(tool)
  if (!method || !METHOD_TO_RPC[method]) continue
  const rpc = METHOD_TO_RPC[method]
  const declared = declaredProperties(tool)
  const returned = daemonKeys(rpc)
  if (!declared || !returned) continue
  // The plugin renames the daemon's id to containerId on the way out.
  // The plugin renames the daemon's id to containerId on the way out -- but
  // only if it picks the fields. Spreading the whole answer drags that id in
  // under a second name, which additionalProperties:false then refuses, so a
  // spread next to a daemon that answers with id is its own finding.
  const seg = plugin.slice(at0(tool), at0(tool) + 4000)
  const execute = seg.slice(seg.indexOf('async execute'))
  const spreads = /\.\.\.result/.test(execute)
  const missing = [...returned].filter((k) => k !== 'id' && !declared.props.has(k))
  const dragsId = spreads && returned.has('id') && !declared.props.has('id')
  // A required field the daemon never returns is undefined at runtime, and a
  // required field that is undefined is how box_type_text failed while its
  // TypeScript type claimed the field was there. Only a field the execute body
  // names as its own counts as filled.
  const derived = new Set([...execute.matchAll(/([A-Za-z_][A-Za-z0-9_]*):/g)].map((x) => x[1]))
  const unbacked = [...declared.required].filter(
    (k) => !returned.has(k) && k !== 'containerId' && !derived.has(k),
  )
  checked++
  if (missing.length === 0 && !dragsId && unbacked.length === 0) {
    if (verbose) console.log('  ok   ' + tool)
    continue
  }
  bad++
  console.log('  BAD  ' + tool + '  (' + method + ' -> ' + rpc + ')')
  if (missing.length) console.log('        daemon returns, schema does not declare: ' + missing.join(', '))
  if (dragsId) console.log('        execute spreads the answer, so the daemon id reaches the output undeclared')
  if (unbacked.length) console.log('        required, but the daemon never returns it and execute never sets it: ' + unbacked.join(', '))
}

console.log('\n' + checked + ' tool(s) reconciled, ' + bad + ' mismatch(es)')
process.exit(bad > 0 ? 1 : 0)
