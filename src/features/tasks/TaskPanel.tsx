import { useEffect, useRef, useState } from 'react'
import type { TaskRecord } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Dialog } from '../../ui/Dialog'
import { Stack } from '../../ui/Stack'

type TaskText = { tasks: string; taskRunning: string; recentTasks: string; cancel: string; retry: string; viewLog: string; close: string; refresh: string; remove: string; confirmTitle: string; dialogCancel: string; dialogConfirm: string; deleteTaskConfirm: string }

export function TaskPanel({ tasks, logs, text, onClose, onCancel, onRetry, onLog, onDelete }: { tasks: TaskRecord[]; logs: Record<string, string>; text: TaskText; onClose: () => void; onCancel: (id: string) => Promise<void>; onRetry: (id: string) => Promise<void>; onLog: (id: string) => Promise<void>; onDelete: (id: string) => Promise<void> }) {
  const [expanded, setExpanded] = useState<string | null>(null)
  const logRef = useRef<HTMLPreElement | null>(null)
  useEffect(() => {
    // Keep an expanded log pinned to the newest line so live updates stay
    // visible without manual scrolling.
    if (expanded !== null && logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight
    }
  }, [expanded, logs])
  const [page, setPage] = useState(0)
  const [pendingDelete, setPendingDelete] = useState<string | null>(null)
  const pageSize = 6
  const active = tasks.filter((task) => task.status === 'queued' || task.status === 'running' || task.status === 'waiting_input')
  const recent = tasks.filter((task) => !active.includes(task))
  const pageCount = Math.max(1, Math.ceil(recent.length / pageSize))
  useEffect(() => { setPage((current) => Math.min(current, pageCount - 1)) }, [pageCount])
  const currentPage = Math.min(page, pageCount - 1)
  const paged = recent.slice(currentPage * pageSize, (currentPage + 1) * pageSize)
  const terminal = (task: TaskRecord) => !['queued', 'running', 'waiting_input'].includes(task.status)

  async function confirmDelete(): Promise<void> {
    if (pendingDelete === null) return
    const id = pendingDelete
    setPendingDelete(null)
    setPage(0)
    await onDelete(id)
  }

  const renderTask = (task: TaskRecord) => (
    <Card key={task.id} padding="sm">
      <div className="ui-toolbar justify-between gap-3">
        <Stack gap={2} className="task-copy">
          <strong>{task.kind.replaceAll('-', ' ')}</strong>
          <span>{task.stage} · {task.resourceKeys.join(', ')}</span>
          {task.error !== null && <span className="task-error">{task.error}</span>}
          <div className="task-progress"><i style={{ width: `${task.progress}%` }} /></div>
          {expanded === task.id && <div className="task-inline-log">
            <div className="task-inline-log-head"><span>{text.viewLog}</span><Button variant="secondary" size="sm" onClick={() => { void onLog(task.id) }}>{text.refresh}</Button></div>
            <pre ref={logRef}>{logs[task.id] || 'No log output yet.'}</pre>
          </div>}
        </Stack>
        <Stack gap={2} className="task-actions">
          <Badge variant={task.status === 'succeeded' ? 'success' : task.status === 'failed' || task.status === 'cancelled' || task.status === 'interrupted' ? 'danger' : 'primary'}>{task.status}</Badge>
          {(task.status === 'queued' || task.status === 'running') && <Button variant="secondary" size="sm" onClick={() => { void onCancel(task.id) }}>{text.cancel}</Button>}
          {['failed', 'cancelled', 'interrupted'].includes(task.status) && <Button variant="secondary" size="sm" onClick={() => { void onRetry(task.id) }}>{text.retry}</Button>}
          {terminal(task) && <Button variant="danger" size="sm" onClick={() => { setPendingDelete(task.id) }}>{text.remove}</Button>}
          <Button variant="secondary" size="sm" onClick={() => { if (expanded !== task.id) void onLog(task.id); setExpanded((current) => current === task.id ? null : task.id) }}>{text.viewLog}</Button>
        </Stack>
      </div>
    </Card>
  )

  return (
    <aside className="task-panel" aria-label={text.tasks}>
      <div className="task-panel-header">
        <div>
          <p className="eyebrow">{text.tasks}</p>
          <h2>{active.length} {text.taskRunning}</h2>
        </div>
        <Button variant="secondary" size="sm" onClick={onClose}>{text.close}</Button>
      </div>
      {active.length > 0 && <Stack gap={3}>{active.map(renderTask)}</Stack>}
      <p className="task-group-title">{text.recentTasks}</p>
      <Stack gap={3}>
        {paged.map(renderTask)}
        {recent.length === 0 && <p className="field-help">No task history.</p>}
      </Stack>
      {recent.length > pageSize && (
        <div className="task-pager">
          <Button variant="secondary" size="sm" disabled={currentPage === 0} onClick={() => { setPage(currentPage - 1) }}>‹</Button>
          <span>{currentPage + 1} / {pageCount}</span>
          <Button variant="secondary" size="sm" disabled={currentPage >= pageCount - 1} onClick={() => { setPage(currentPage + 1) }}>›</Button>
        </div>
      )}
      <Dialog
        open={pendingDelete !== null}
        title={text.confirmTitle}
        description={text.deleteTaskConfirm}
        onClose={() => { setPendingDelete(null) }}
      >
        <div className="ui-dialog-actions">
          <Button variant="secondary" size="sm" onClick={() => { setPendingDelete(null) }}>{text.dialogCancel}</Button>
          <Button variant="danger" size="sm" onClick={() => { void confirmDelete() }}>{text.dialogConfirm}</Button>
        </div>
      </Dialog>
    </aside>
  )
}
