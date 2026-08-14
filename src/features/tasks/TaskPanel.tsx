import { useEffect, useRef, useState } from 'react'
import type { TaskRecord } from '../../shared/types/domain'

type TaskText = { tasks: string; taskRunning: string; recentTasks: string; cancel: string; retry: string; viewLog: string; close: string; refresh: string; remove: string }

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
  const pageSize = 6
  const active = tasks.filter((task) => task.status === 'queued' || task.status === 'running' || task.status === 'waiting_input')
  const recent = tasks.filter((task) => !active.includes(task))
  const pageCount = Math.max(1, Math.ceil(recent.length / pageSize))
  useEffect(() => { setPage((current) => Math.min(current, pageCount - 1)) }, [pageCount])
  const currentPage = Math.min(page, pageCount - 1)
  const paged = recent.slice(currentPage * pageSize, (currentPage + 1) * pageSize)
  const terminal = (task: TaskRecord) => !['queued', 'running', 'waiting_input'].includes(task.status)
  const renderTask = (task: TaskRecord) => (
    <article className="task-row" key={task.id}>
      <div className="task-copy">
        <strong>{task.kind.replaceAll('-', ' ')}</strong>
        <span>{task.stage} · {task.resourceKeys.join(', ')}</span>
        {task.error !== null && <span className="task-error">{task.error}</span>}
        <div className="task-progress"><i style={{ width: `${task.progress}%` }} /></div>
        {expanded === task.id && <div className="task-inline-log"><div className="task-inline-log-head"><span>{text.viewLog}</span><button type="button" className="secondary" onClick={() => { void onLog(task.id) }}>{text.refresh}</button></div><pre ref={logRef}>{logs[task.id] || 'No log output yet.'}</pre></div>}
      </div>
      <div className="task-actions">
        <span className={`task-status ${task.status}`}>{task.status}</span>
        {(task.status === 'queued' || task.status === 'running') && <button type="button" className="secondary" onClick={() => { void onCancel(task.id) }}>{text.cancel}</button>}
        {['failed', 'cancelled', 'interrupted'].includes(task.status) && <button type="button" className="secondary" onClick={() => { void onRetry(task.id) }}>{text.retry}</button>}
        {terminal(task) && <button type="button" className="secondary danger-button" onClick={() => { if (window.confirm(`Delete ${task.kind} task?`)) { setPage(0); void onDelete(task.id) } }}>{text.remove}</button>}
        <button type="button" className="secondary" onClick={() => { if (expanded !== task.id) void onLog(task.id); setExpanded((current) => current === task.id ? null : task.id) }}>{text.viewLog}</button>
      </div>
    </article>
  )
  return <aside className="task-panel" aria-label={text.tasks}><div className="task-panel-header"><div><p className="eyebrow">{text.tasks}</p><h2>{active.length} {text.taskRunning}</h2></div><button type="button" className="secondary" onClick={onClose}>{text.close}</button></div>{active.length > 0 && <section className="task-group">{active.map(renderTask)}</section>}<p className="task-group-title">{text.recentTasks}</p><section className="task-group">{paged.map(renderTask)}{recent.length === 0 && <p className="field-help">No task history.</p>}</section>{recent.length > pageSize && <div className="task-pager"><button type="button" className="secondary" disabled={currentPage === 0} onClick={() => { setPage(currentPage - 1) }}>‹</button><span>{currentPage + 1} / {pageCount}</span><button type="button" className="secondary" disabled={currentPage >= pageCount - 1} onClick={() => { setPage(currentPage + 1) }}>›</button></div>}</aside>
}
