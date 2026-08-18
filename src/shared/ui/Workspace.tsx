import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Stack } from '../../ui/Stack'

export function Workspace({ eyebrow, title, note, empty, action, onAction }: { eyebrow: string; title: string; note: string; empty: string; action: string; onAction: () => void }) {
  return (
    <section className="workspace">
      <p className="eyebrow">{eyebrow}</p>
      <h1>{title}</h1>
      <p className="workspace-note">{note}</p>
      <Card>
        <Stack gap={4}>
          <span>{empty}</span>
          <div><Button variant="primary" size="lg" onClick={onAction}>{action}</Button></div>
        </Stack>
      </Card>
    </section>
  )
}

export function DirectoryCard({ label, value, action, onChoose }: { label: string; value: string; action: string; onChoose: () => Promise<void> }) {
  return (
    <Card>
      <div className="directory-card-body">
        <div>
          <p className="label">{label}</p>
          <p className="path">{value}</p>
        </div>
        <Button variant="primary" onClick={() => { void onChoose() }}>{action}</Button>
      </div>
    </Card>
  )
}
