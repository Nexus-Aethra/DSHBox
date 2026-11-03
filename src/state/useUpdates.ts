import { useCallback, useEffect, useRef, useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { UpdatePolicy, UpdateState } from '../shared/types/domain'

/**
 * The update surface, shared by the Settings pane and the badge in the shell.
 *
 * Reads are cheap: the daemon answers from the record it already wrote, and
 * only touches the network when its day-long throttle allows. So opening the
 * pane costs nothing, and a closed window costs nothing either -- the daemon
 * checks on its own schedule, because it is the process that is still running.
 *
 * Every write here is a command a person asked for.
 */

/** Poll interval while a download task runs. */
const TASK_POLL_MS = 600

/**
 * The daemon's stage strings are English task prose; the pane localizes the
 * ones it knows and falls back to the raw text for anything new. Exported
 * pure so the mapping, not the daemon's wording, is what a test can pin.
 */
export type DownloadStageKey = 'manifest' | 'downloading' | 'ready' | 'other'

export function stageOf(stage: string): { key: DownloadStageKey; detail: string | null } {
  if (stage === 'reading the release manifest') return { key: 'manifest', detail: null }
  if (stage === '') return { key: 'downloading', detail: null }
  if (stage.startsWith('downloading')) {
    const detail = stage.slice('downloading'.length).trim()
    return { key: 'downloading', detail: detail === '' ? null : detail }
  }
  if (stage === 'ready') return { key: 'ready', detail: null }
  return { key: 'other', detail: stage }
}
/**
 * How long to keep waiting before giving the loop up: a download task can
 * legitimately take minutes on a slow mirror, but a loop without a deadline
 * hammers the daemon forever if a task never settles.
 */
const TASK_DEADLINE_MS = 30 * 60 * 1000

type DownloadProgress = { stage: string; progress: number }

/**
 * Wait for one daemon task to settle, reporting its stage as it moves.
 *
 * The download is a scheduler task rather than a blocking RPC, so the pane
 * waits on the task record. The terminal states are the scheduler's own
 * (succeeded / failed / cancelled / interrupted); there is no "completed",
 * and treating one as the other would hang here forever.
 */
async function waitForTask(
  id: string,
  onProgress: (progress: DownloadProgress) => void,
  alive: () => boolean,
): Promise<void> {
  const deadline = Date.now() + TASK_DEADLINE_MS
  for (;;) {
    // The pane closed: stop polling. The task itself keeps running in the
    // daemon and its result shows up on the next open -- a loop that outlives
    // its caller is just noise against the daemon.
    if (!alive()) throw new Error('the updates pane closed while the download ran')
    const tasks = await boxApi.listTasks()
    const task = tasks.find((entry) => entry.id === id)
    if (task === undefined) throw new Error('the download task disappeared from the queue')
    onProgress({ stage: task.stage, progress: task.progress })
    if (task.status === 'succeeded') return
    if (task.status === 'failed' || task.status === 'cancelled' || task.status === 'interrupted') {
      throw new Error(task.error ?? 'the download did not finish')
    }
    if (Date.now() > deadline) throw new Error('the download took too long; check the task panel')
    await new Promise((resolve) => setTimeout(resolve, TASK_POLL_MS))
  }
}

export function useUpdates(onError: (message: string | null) => void) {
  const [state, setState] = useState<UpdateState | null>(null)
  const [checking, setChecking] = useState(false)
  const [busy, setBusy] = useState(false)
  const [downloading, setDownloading] = useState<DownloadProgress | null>(null)
  // False once the component unmounts: waitForTask stops instead of polling
  // from beyond the grave.
  const aliveRef = useRef(true)
  useEffect(() => () => { aliveRef.current = false }, [])

  const reload = useCallback(async () => {
    try {
      setState(await boxApi.getUpdateState())
    } catch (reason) {
      onError(String(reason))
    }
  }, [onError])

  // Load once on mount. That first call is also what nudges the daemon's
  // background check, so a user who opens Settings once gets an answer without
  // pressing anything.
  useEffect(() => { void reload() }, [reload])

  // The daemon announces a finished check (the background pass and the pane's
  // own button both do): refresh instead of showing a state that is already
  // stale while the pane sits open.
  useEffect(() => {
    const unlisten = boxApi.listenTask<{ event: string }>('daemon://event', (payload) => {
      if (payload.event === 'update_state') void reload()
    })
    return () => { void unlisten.then((unlisten) => unlisten()) }
  }, [reload])

  const check = useCallback(async () => {
    setChecking(true)
    try {
      setState(await boxApi.checkForUpdate())
      onError(null)
    } catch (reason) {
      onError(String(reason))
    } finally {
      setChecking(false)
    }
  }, [onError])

  const setPolicy = useCallback(async (policy: UpdatePolicy) => {
    setBusy(true)
    try {
      await boxApi.setUpdatePolicy(policy)
      await reload()
    } catch (reason) {
      onError(String(reason))
    } finally {
      setBusy(false)
    }
  }, [onError, reload])

  const skip = useCallback(async () => {
    const version = state?.available?.version
    if (version === undefined) return
    setBusy(true)
    try {
      await boxApi.skipUpdateVersion(version)
      await reload()
    } catch (reason) {
      onError(String(reason))
    } finally {
      setBusy(false)
    }
  }, [onError, reload, state])

  const unskip = useCallback(async () => {
    setBusy(true)
    try {
      await boxApi.clearUpdateSkip()
      await reload()
    } catch (reason) {
      onError(String(reason))
    } finally {
      setBusy(false)
    }
  }, [onError, reload])

  /**
   * Fetch this machine's installer and verify it.
   *
   * A daemon task, so it has a log and a cancel button like every other long
   * operation. The pane re-reads afterwards, which is when the verified path
   * appears; a failure leaves no file behind, so "ready" always means checked.
   */
  const download = useCallback(async () => {
    setBusy(true)
    setDownloading({ stage: '', progress: 0 })
    try {
      const task = await boxApi.downloadUpdate(state?.available?.version ?? null)
      await waitForTask(task.id, setDownloading, () => aliveRef.current)
      await reload()
      onError(null)
    } catch (reason) {
      onError(String(reason))
    } finally {
      setDownloading(null)
      setBusy(false)
    }
  }, [onError, reload, state])

  const discard = useCallback(async () => {
    setBusy(true)
    try {
      await boxApi.discardUpdateDownload(state?.download?.version ?? null)
      await reload()
    } catch (reason) {
      onError(String(reason))
    } finally {
      setBusy(false)
    }
  }, [onError, reload, state])

  return { state, checking, busy, downloading, reload, check, setPolicy, skip, unskip, download, discard }
}
