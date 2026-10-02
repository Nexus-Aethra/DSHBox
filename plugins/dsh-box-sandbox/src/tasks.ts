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
  /** Succeeded, Failed, RollingBack, RolledBack, Cancelled, Interrupted, Queued, Running. */
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

/** States the daemon will not move out of. */
const TERMINAL = new Set(['Succeeded', 'Cancelled', 'Interrupted', 'RolledBack'])

/**
 * True once a task will not change again.
 *
 * `Failed` is the exception the daemon's own docs call out: a failed task
 * rolls back, and only a failed *rollback* ends it. The record has no other
 * field that distinguishes the two, so the presence of `rollbackError` is what
 * says this failure is the end of it.
 */
export function isFinished(task: TaskRecord): boolean {
  if (TERMINAL.has(task.status)) return true
  if (task.status === 'Failed') {
    const rollback = task.rollbackError
    return typeof rollback === 'string' && rollback.length > 0
  }
  return false
}

/** True when the task stopped without doing what was asked. */
export function failed(task: TaskRecord): boolean {
  return task.status === 'Failed' || task.status === 'Interrupted' || task.status === 'RolledBack'
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
