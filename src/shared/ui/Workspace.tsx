export function Workspace({ eyebrow, title, note, empty, action }: { eyebrow: string; title: string; note: string; empty: string; action: string }) {
  return <section className="workspace"><p className="eyebrow">{eyebrow}</p><h1>{title}</h1><p className="workspace-note">{note}</p><section className="card empty-card"><span>{empty}</span><button type="button" className="primary">{action}</button></section></section>
}

export function DirectoryCard({ label, value, action, onChoose }: { label: string; value: string; action: string; onChoose: () => Promise<void> }) {
  return <section className="card"><div><p className="label">{label}</p><p className="path">{value}</p></div><button type="button" className="primary" onClick={() => { void onChoose() }}>{action}</button></section>
}
