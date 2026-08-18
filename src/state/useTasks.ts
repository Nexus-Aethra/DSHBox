import { useEffect, useRef, useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { TaskRecord } from '../shared/types/domain'

export type TaskRefreshers = {
  onVersionsChanged: () => void
  onTemplatesChanged: () => void
  onContainersChanged: () => void
  onRepositoryChanged: () => void
  onBundlesChanged: () => void
  onContainerDetailsChanged: (containerId: string) => Promise<void>
}

export function useTasks(refreshers: TaskRefreshers, onError: (message: string | null) => void) {
  const [tasks, setTasks] = useState<TaskRecord[]>([])
  const [taskLogs, setTaskLogs] = useState<Record<string, string>>({})
  const [taskPanelOpen, setTaskPanelOpen] = useState(false)

  // Keep the latest callbacks without re-subscribing to the event bus on
  // every render; the listeners below run for the lifetime of the page.
  const refreshersRef = useRef(refreshers)
  refreshersRef.current = refreshers

  useEffect(() => {
    void boxApi.listTasks().then(setTasks).catch(() => undefined)
    const update = (task: TaskRecord) => setTasks((current) => [task, ...current.filter((item) => item.id !== task.id)])
    const refreshForTask = (task: TaskRecord) => {
      if (task.status !== 'succeeded') return
      const current = refreshersRef.current
      if (task.kind === 'dsh-version-install' || task.kind === 'template-pull' || task.kind === 'dsh-catalog-refresh') current.onVersionsChanged()
      // A pulled template registers itself in the template index, so the
      // Resources > Template list (and the container-creation picker) must
      // reload right after the pull settles. A finished build registers a
      // built template in the same index.
      if (task.kind === 'template-pull' || task.kind === 'image-build') current.onTemplatesChanged()
      // `template-container` creates (and starts) a brand-new container,
      // but its kind does not carry the `container-` prefix the daemon
      // uses for lifecycle tasks, so it needs an explicit mention or the
      // container list never refreshes after creation.
      if (task.kind.startsWith('container-') || task.kind === 'image-build' || task.kind === 'template-container') current.onContainersChanged()
      if ((task.kind === 'container-extension-add' || task.kind === 'container-extension-copy' || task.kind === 'container-bundle-install') && task.resourceKeys.some((key) => key.startsWith('container:'))) {
        const containerId = task.resourceKeys.find((key) => key.startsWith('container:'))?.slice('container:'.length)
        if (containerId !== undefined) void current.onContainerDetailsChanged(containerId).catch((reason: unknown) => { onError(String(reason)) })
      }
      if (task.kind === 'repository-extension-import' || task.kind === 'repository-extension-export' || task.kind === 'workspace-extension-import' || task.kind === 'bundle-import' || task.kind === 'image-build') {
        current.onRepositoryChanged()
        if (task.kind === 'bundle-import') current.onBundlesChanged()
      }
    }
    const taskEvents = ['task://created', 'task://updated', 'task://finished'].map((event) => boxApi.listenTask<TaskRecord>(event, (payload) => { update(payload); refreshForTask(payload) }))
    const logEvent = boxApi.listenTask<{ taskId: string; line: string }>('task://log', (payload) => setTaskLogs((current) => payload.taskId in current ? { ...current, [payload.taskId]: `${current[payload.taskId]}${current[payload.taskId] ? '\n' : ''}${payload.line}` } : current))
    const unlisteners = Promise.all([...taskEvents, logEvent])
    // Poll for tasks enqueued by the CLI process (which cannot emit Tauri
    // events into this process). The backend merges the shared state file on
    // every call, so the next poll picks up progress and completion too.
    const pollInterval = setInterval(() => {
      void boxApi.listTasks().then((latest) => setTasks((current) => {
        const currentMap = new Map(current.map((t) => [t.id, t]))
        for (const task of latest) {
          const existing = currentMap.get(task.id)
          if (!existing || existing.status !== task.status || existing.progress !== task.progress) {
            currentMap.set(task.id, task)
            if (task.status === 'succeeded' && (!existing || existing.status !== 'succeeded')) {
              refreshForTask(task)
            }
          }
        }
        return [...currentMap.values()].sort((a, b) => b.createdAt - a.createdAt)
      })).catch(() => undefined)
    }, 3000)
    return () => {
      clearInterval(pollInterval)
      void unlisteners.then((items) => items.forEach((unlisten) => unlisten()))
    }
  }, [])

  async function cancelTask(id: string): Promise<void> {
    try { await boxApi.cancelTask(id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function retryTask(id: string): Promise<void> {
    try { await boxApi.retryTask(id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function deleteTask(id: string): Promise<void> {
    try { await boxApi.deleteTask(id); setTasks(await boxApi.listTasks()); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function showTaskLog(id: string): Promise<void> {
    try { const content = await boxApi.readTaskLog(id); setTaskLogs((current) => ({ ...current, [id]: content })); setTaskPanelOpen(true) } catch (reason) { onError(String(reason)) }
  }

  return { tasks, taskLogs, taskPanelOpen, setTaskPanelOpen, cancelTask, retryTask, deleteTask, showTaskLog }
}
