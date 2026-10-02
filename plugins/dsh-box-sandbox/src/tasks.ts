/**
 * Following a dshbox background task to its outcome.
 *
 * Most of what an agent wants to do to a box -- start a container, build a
 * template, inject a resource -- is not an operation but a *task*: the daemon
 * answers with a task record and then does the work. That split is right for a
 * UI, which can watch a progress bar. For a caller with no other channel it
 * means one call returns an id and no outcome, and the next thing to write is
 * the polling loop. That loop is the learning cost this removes.
 *
 * So the tools enqueue and wait together, and hand back the finished record:
 * state, progress, the log path, and the error when there was one.
 *
 * The state machine is the part worth being careful about. A task that fails
 * may still be running: it rolls back first, and only then is finished. So
 * `failed` alone does not mean done -- treating it as terminal returns while
 * rollback is still releasing locks, and the caller then starts the next
 * operation against a store that is still being unwound.
 *
 * @module dsh-box-sandbox/tasks
 */

import { getRpc } from './rpc'

/** One task as the daemon reports it. */
export interface TaskRecord {
  id: string
  kind: string
  /** Lower case, as the daemon serialises it: succeeded, failed, rollingback, rolledback, cancelled, interrupted, queued, running. */
  status: string
  /** A human-readable stage label such as Completed. */
  stage: string
  progress: number
  error: string | null
  /** Set only when a failure's own rollback also failed. */
  rollbackError?: string | null
  /** Path of the task's log, which is where the detail behind an error lives. */
  logPath?: string
  startedAt?: number
  finishedAt?: number
}

/**
 * The daemon's wire form for a task state.
 *
 * `TaskState` is declared in Rust with `#[serde(rename_all = "lowercase")]`, so
 * what crosses the wire is `succeeded`, `rolledback`, `cancelled` -- not the
 * PascalCase the enum spells in source. Comparing against the Rust spelling
 * matches nothing: a finished task is not recognised as finished, so a poll
 * runs to its full budget and then reports a task that is sitting at 100%
 * successful as one that did not finish. Normalising here rather than at each
 * call site is also what makes the two separators irrelevant, so a future
 * `rolling_back` cannot reintroduce the same silence a different way.
 */
function state(task: TaskRecord): string {
  return task.status.trim().toLowerCase().replace(/[\s_-]/g, '')
}

/** States the daemon will not move out of. */
const TERMINAL = new Set(['succeeded', 'cancelled', 'interrupted', 'rolledback'])

/**
 * True once a task will not change again.
 *
 * `Failed` is the exception the daemon's own docs call out: a failed task rolls
 * back, and only a failed *rollback* ends it. So `failed` on its own is not
 * the end of a task -- polling on it alone waits out the budget and then reports
 * a task that stopped as one that never stopped.
 *
 * What actually says the failure is over is the daemon's own `finishedAt`: it is
 * stamped when the task, rollback included, is done, and it stays null while a
 * rollback is still releasing locks. A task carrying one will not be handed to a
 * worker again, so treating it as finished is not a guess about the state
 * machine -- it is reading the same field the scheduler writes.
 */
export function isFinished(task: TaskRecord): boolean {
  const current = state(task)
  if (TERMINAL.has(current)) return true
  if (current === 'failed') {
    const rollback = task.rollbackError
    if (typeof rollback === 'string' && rollback.length > 0) return true
    return typeof task.finishedAt === 'number' && task.finishedAt > 0
  }
  return false
}

/** True when the task stopped without doing what was asked. */
export function failed(task: TaskRecord): boolean {
  const current = state(task)
  return current === 'failed' || current === 'interrupted' || current === 'rolledback'
}

export interface WaitOptions {
  /** Give up after this many milliseconds. Default 10 minutes. */
  timeoutMs?: number
  /** Pause between polls. Default 700ms. */
  intervalMs?: number
}

/**
 * Poll a task until it finishes.
 *
 * A timeout is reported as such rather than as a result: handing back a task
 * that is still Queued, under a heading that reads like an outcome, is how a
 * caller goes on to assume something that has not happened yet. The last
 * known state comes back with it so the caller can say where it got to.
 */
export async function waitForTask(
  id: string,
  options: WaitOptions = {},
): Promise<{ finished: boolean; task: TaskRecord }> {
  const timeoutMs = options.timeoutMs ?? 600_000
  const intervalMs = options.intervalMs ?? 700
  const deadline = Date.now() + timeoutMs
  const rpc = getRpc()
  for (;;) {
    const task = await rpc.call<TaskRecord>('task_status', { id })
    if (isFinished(task)) return { finished: true, task }
    if (Date.now() >= deadline) return { finished: false, task }
    await new Promise((resolve) => setTimeout(resolve, intervalMs))
  }
}

/**
 * Enqueue a task and follow it to the end.
 *
 * Returns the finished record, or throws with the daemon's own error text once
 * the task is finished and failed. The log path rides along in the message
 * because that is where the detail is, and an agent that has to go looking for
 * it is an agent that will not.
 */
export async function runTask(
  method: string,
  params: Record<string, unknown>,
  options: WaitOptions = {},
): Promise<TaskRecord> {
  const rpc = getRpc()
  const enqueued = await rpc.call<TaskRecord>(method, params)
  const { finished, task } = await waitForTask(enqueued.id, options)
  if (!finished) {
    throw new Error(
      `the ${task.kind} task did not finish within the budget; it is still ${task.status}`
        + ` at ${task.progress}%. Watch it with box_task, or read ${task.logPath ?? 'its log'}.`,
    )
  }
  if (failed(task)) {
    throw new Error(
      `the ${task.kind} task ended ${task.status}`
        + (task.error ? `: ${task.error}` : '')
        + (task.rollbackError ? ` (rollback also failed: ${task.rollbackError})` : '')
        + (task.logPath ? `. Full detail in ${task.logPath}` : '.'),
    )
  }
  return task
}
