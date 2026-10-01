import { readFileSync, writeFileSync } from 'node:fs'
import { DshboxRpc, DshboxUnavailableError, discoveryPath } from './dist/rpc.mjs'

const rpc = new DshboxRpc()
let pass = 0
let fail = 0

function check(label, ok, detail) {
  if (ok) { pass++; console.log('  PASS  ' + label) }
  else { fail++; console.log('  FAIL  ' + label + ' -> ' + detail) }
}

console.log('discovery path: ' + discoveryPath(rpc.configDir))
console.log('')
console.log('1. discovery + ping')
try {
  const record = rpc.discovery()
  console.log('   record: ' + JSON.stringify(record))
  const pong = await rpc.ping()
  console.log('   ping  : ' + JSON.stringify(pong))
  check('ping reaches daemon', pong.status === 'running', JSON.stringify(pong))
  check('daemon pid matches record', pong.pid === record.pid, 'record=' + record.pid + ' pong=' + pong.pid)
} catch (e) {
  check('discovery + ping', false, e.message)
}

console.log('')
console.log('2. sync method (task_state_machine)')
try {
  const sm = await rpc.call('task_state_machine')
  check('sync method returns an object', sm !== null && typeof sm === 'object', typeof sm)
} catch (e) {
  check('sync method', false, e.message)
}

console.log('')
console.log('3. daemon-reported error surfaces (not treated as outage)')
try {
  await rpc.call('definitely_not_a_method')
  check('unknown method throws', false, 'no error thrown')
} catch (e) {
  check('unknown method throws', e instanceof Error, e.message)
  check('message comes from daemon', !e.message.includes('unreachable'), e.message)
}

console.log('')
console.log('4. missing daemon -> DshboxUnavailableError naming the path')
try {
  const missing = new DshboxRpc({ configDir: 'D:/definitely-not-a-config-dir' })
  await missing.ping()
  check('missing discovery throws', false, 'no error thrown')
} catch (e) {
  check('throws DshboxUnavailableError', e instanceof DshboxUnavailableError, e.constructor.name)
  check('message names the consulted path', e.message.includes('definitely-not-a-config-dir'), e.message)
}

console.log('')
console.log('5. stale port: re-read once, then fail cleanly')
const path = discoveryPath(rpc.configDir)
const original = readFileSync(path, 'utf8')
try {
  const good = rpc.discovery()
  writeFileSync(path, JSON.stringify({ ...good, port: 1 }))
  const started = Date.now()
  try {
    await new DshboxRpc({ timeoutMs: 3000 }).ping()
    check('stale port does not succeed', false, 'unexpectedly succeeded')
  } catch (e) {
    check('stale port fails as an Error', e instanceof Error, e.message)
    check('fails fast, no hang', Date.now() - started < 15000, Date.now() - started + 'ms')
  }
} finally {
  writeFileSync(path, original)
}

console.log('')
console.log('6. recovery after restoring the real record')
try {
  const pong = await rpc.ping()
  check('recovered', pong.status === 'running', JSON.stringify(pong))
} catch (e) {
  check('recovered', false, e.message)
}

console.log('')
console.log('result: ' + pass + ' passed, ' + fail + ' failed')
process.exit(fail === 0 ? 0 : 1)
