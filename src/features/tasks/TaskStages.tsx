import { useEffect, useRef, useState } from 'react'
import type { TaskRecord, TaskStage } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Dialog } from '../../ui/Dialog'
import { Stack } from '../../ui/Stack'

type StageText = { stages: string; stageElapsed: string; stageBudget: string; stageRetries: string; stageFailedAt: string }

const ACTIVE = ['queued', 'running', 'waiting_input']

/** Elapsed seconds of a stage, measured to its finish or to now. */
function stageElapsed(stage: TaskStage, now: number): number {
  if (stage.startedAt === null) return 0
  const end = stage.finishedAt ?? now
  return Math.max(0, end - stage.startedAt)
}

function formatDuration(seconds: number): string {
  const total = Math.max(0, Math.round(seconds))
  const minutes = Math.floor(total / 60)
  const rest = total % 60
  if (minutes === 0) return `${rest}s`
  return `${minutes}m${rest.toString().padStart(2, '0')}s`
}

/**
 * One stage as a row with its own bar.
 *
 * The whole point of the per-stage view: a single overall bar sits in one
 * place for seven minutes and cannot be told apart from a crash. A row per
 * stage shows which step is running, how long it has been running, and how
 * long it was expected to take — so "slow" and "stuck" stop looking alike.
 */
function StageRow({ stage, now, text }: { stage: TaskStage; now: number; text: StageText }) {
  const elapsed = stageElapsed(stage, now)
  const budget = stage.durationHintSecs
  const overBudget = budget > 0 && elapsed > budget && stage.state === 'running'
  const marker = stage.state === 'done' ? '✓' : stage.state === 'failed' ? '✕' : stage.state === 'running' ? '▸' : stage.state === 'skipped' ? '–' : '○'
  return (
    <div className={`task-stage task-stage-${stage.state}`}>
      <div className="task-stage-head">
        <span className="task-stage-marker" aria-hidden="true">{marker}</span>
        <span className="task-stage-label">{stage.label}</span>
        <span className="task-stage-pct">{stage.progress}%</span>
      </div>
      <div className="task-stage-bar" role="progressbar" aria-valuenow={stage.progress} aria-valuemin={0} aria-valuemax={100} aria-label={stage.label}>
        <i style={{ width: `${stage.progress}%` }} />
      </div>
      <div className="task-stage-meta">
        {stage.startedAt !== null && <>
          <span>{text.stageElapsed} {formatDuration(elapsed)}</span>
          {budget > 0 && <span className={overBudget ? 'task-stage-over' : ''}>{text.stageBudget} ~{formatDuration(budget)}</span>}
        </>}
        {stage.attempts > 0 && <span>{text.stageRetries} {stage.attempts}</span>}
      </div>
      {stage.state === 'failed' && stage.failureDetail && (
        <div className="task-stage-failure"><span>{text.stageFailedAt}</span> {stage.failureDetail}</div>
      )}
    </div>
  )
}

function StageList({ stages, text }: { stages: TaskStage[]; text: StageText }) {
  // A clock for the elapsed counters. It only runs while a stage is live, so
  // a finished task holds no timer.
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))
  const running = stages.some((stage) => stage.state === 'running')
  useEffect(() => {
    if (!running) return
    const timer = window.setInterval(() => { setNow(Math.floor(Date.now() / 1000)) }, 1000)
    return () => { window.clearInterval(timer) }
  }, [running])
  return (
    <div className="task-stages" aria-label={text.stages}>
      {stages.map((stage) => <StageRow key={stage.id} stage={stage} now={now} text={text} />)}
    </div>
  )
}

export { StageList, formatDuration }