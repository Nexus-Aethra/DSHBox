import { useState } from 'react'
import type { EnrollReply, NodeRecord } from '../../shared/types/domain'
import type { Text } from '../../i18n'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Stack } from '../../ui/Stack'

type Props = {
  nodes: NodeRecord[]
  loading: boolean
  removingId: string | null
  enrolled: EnrollReply | null
  enrolling: boolean
  onEnroll: (kind: string, platform: string, nodeId: string | null) => void
  onRemove: (id: string) => void
  onDismissEnrolled: () => void
  text: Text
}

/**
 * The Nodes page: the registry, an enrol form, and the one-time secret.
 *
 * The secret panel is the reason this page is not just a list. An enrol reply
 * carries the bootstrap seed exactly once -- the daemon stores only the chain
 * end, so a copy the user does not take here is gone. The panel is therefore
 * a copy field with a note, not a toast that disappears.
 */
export function NodesPage(props: Props) {
  const { nodes, loading, removingId, enrolled, enrolling, onEnroll, onRemove, onDismissEnrolled, text } = props
  const [kind, setKind] = useState('wsl2')
  const [platform, setPlatform] = useState('windows-x86_64')
  const [nodeId, setNodeId] = useState('')
  const [copied, setCopied] = useState(false)

  function statusLabel(status: NodeRecord['status']): string {
    if (status === 'online') return text.nodeStatusOnline
    if (status === 'offline') return text.nodeStatusOffline
    if (status === 'registering') return text.nodeStatusRegistering
    if (status === 'draining') return text.nodeStatusDraining
    return text.nodeStatusFailed
  }

  async function copySecret(seed: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(seed)
      setCopied(true)
    } catch {
      // A clipboard refusal is not worth an error banner: the seed is on
      // screen and selectable, which is the fallback that always works.
    }
  }

  return (
    <section className="workspace">
      <div className="workspace-heading">
        <div>
          <p className="eyebrow">NODE</p>
          <h1>{text.nodesTitle}</h1>
        </div>
      </div>
      <p className="section-note">{text.nodesNote}</p>

      <div className="version-list node-list">
        {nodes.map((node) => (
          <section key={node.id} className="version-row">
            <div className="node-summary">
              <strong>{node.id}</strong>
              <p className="node-meta">
                {node.kind} · {node.platform} · {statusLabel(node.status)}
                {node.lastSeen !== null && ` · ${text.nodeLastSeen} ${new Date(node.lastSeen * 1000).toLocaleString()}`}
              </p>
              {node.capabilities.length > 0 && <p className="node-meta">{node.capabilities.join(', ')}</p>}
            </div>
            <div className="node-actions">
              <Button
                shape="icon"
                variant="ghost"
                className="icon-action"
                aria-label={text.nodeRemove}
                title={text.nodeRemove}
                disabled={removingId === node.id}
                onClick={() => {
                  if (window.confirm(text.nodeRemoveConfirm(node.id))) onRemove(node.id)
                }}
              >
                <span aria-hidden="true" className="remove-icon">×</span>
              </Button>
            </div>
          </section>
        ))}
        {!loading && nodes.length === 0 && <p className="empty-note">{text.noNodes}</p>}
      </div>

      <Card>
        <Stack gap={5}>
          <div>
            <p className="eyebrow">NODE</p>
            <h2>{text.enrollNode}</h2>
          </div>
          <Field label={text.nodeKind} required>
            {(id) => (
              <Input id={id} value={kind} placeholder={text.nodeKindPlaceholder} onChange={(event) => { setKind(event.target.value) }} />
            )}
          </Field>
          <Field label={text.nodePlatform} required>
            {(id) => (
              <Input id={id} value={platform} placeholder={text.nodePlatformPlaceholder} onChange={(event) => { setPlatform(event.target.value) }} />
            )}
          </Field>
          <Field label={text.nodeIdOptional} help={text.nodeIdPlaceholder}>
            {(id) => (
              <Input id={id} value={nodeId} placeholder={text.nodeIdPlaceholder} onChange={(event) => { setNodeId(event.target.value) }} />
            )}
          </Field>
          <div>
            <Button
              variant="primary"
              size="lg"
              loading={enrolling}
              disabled={kind.trim().length === 0 || platform.trim().length === 0}
              onClick={() => { onEnroll(kind.trim(), platform.trim(), nodeId.trim().length === 0 ? null : nodeId.trim()) }}
            >
              {text.enrollNode}
            </Button>
          </div>
        </Stack>
      </Card>

      {enrolled !== null && (
        <Card className="enroll-secret-card">
          <Stack gap={5}>
            <div>
              <p className="eyebrow">NODE</p>
              <h2>{text.enrollSecretTitle}</h2>
              <p className="section-note">{text.enrollSecretNote}</p>
            </div>
            {enrolled.bootstrap !== undefined && (
              <>
                <Field label={text.enrollSecretSeed} required>
                  {(id) => (
                    <Input id={id} readOnly value={enrolled.bootstrap?.seed ?? ''} />
                  )}
                </Field>
                <div className="enroll-secret-actions">
                  <Button
                    variant="primary"
                    size="sm"
                    onClick={() => { void copySecret(enrolled.bootstrap?.seed ?? '') }}
                  >
                    {copied ? text.enrollSecretCopied : text.enrollSecretCopy}
                  </Button>
                  <Button variant="ghost" size="sm" onClick={onDismissEnrolled}>{text.enrollDone}</Button>
                </div>
                <p className="section-note">
                  {text.enrollChainLength}: {enrolled.bootstrap?.chainLength ?? 0}
                </p>
              </>
            )}
            <Field label={text.enrollEnrollToken} required>
              {(id) => <Input id={id} readOnly value={enrolled.enrollToken} />}
            </Field>
            <p className="section-note">{text.enrollTokenNote}</p>
          </Stack>
        </Card>
      )}
    </section>
  )
}
