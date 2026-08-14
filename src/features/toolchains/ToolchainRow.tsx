import type { ToolchainStatus } from '../../shared/types/domain'

export function ToolchainRow({ toolchain, runtimeLabel, notFound, expanded, onToggle }: { toolchain: ToolchainStatus; runtimeLabel: string; notFound: string; expanded: boolean; onToggle: () => void }) {
  const version = toolchain.version
  return <section className={expanded ? 'toolchain-row expanded' : 'toolchain-row'}><div className="toolchain-summary"><strong>{toolchain.name}</strong><code className={version === null ? 'missing' : ''}>{runtimeLabel} · {version ?? notFound}</code></div><div className="toolchain-actions"><button type="button" className="toolchain-toggle" aria-label={expanded ? `Collapse ${toolchain.name}` : `Expand ${toolchain.name}`} onClick={onToggle}>{expanded ? '⌃' : '⌄'}</button></div>{expanded && <div className="toolchain-detail"><p className="label">{runtimeLabel}</p><div className="version-choice"><span>{toolchain.name}</span><code className={version === null ? 'missing' : ''}>{version ?? notFound}</code></div></div>}</section>
}
