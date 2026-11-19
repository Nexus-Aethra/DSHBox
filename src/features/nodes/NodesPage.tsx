import { useMemo, useState } from 'react'
import type { EnrollReply, NodeRecord } from '../../shared/types/domain'
import type { Text } from '../../i18n'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Stack } from '../../ui/Stack'
import { layoutGraph } from '../plugin-graph/layout'
import type { GraphNodeMeta } from '../plugin-graph/PluginGraphView'
import { PluginGraphView } from '../plugin-graph/PluginGraphView'
import { CONTROL_NODE_ID, buildTopology, topologyGraph } from './node-topology'

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

/** How the fleet is presented. The diagram is the default: it is the only
 * view that shows the Box as the hub, which is the fact a list cannot carry. */
type FleetView = 'graph' | 'list'

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
  const [view, setView] = useState<FleetView>('graph')
  const [selected, setSelected] = useState<string | null>(null)

  // The fleet, with the Box as its hub. Recomputed on every render pass
  // rather than memoised on `nodes`, because the cost is one array build
  // and a stale hub would be worse than a redundant one.
  const topology = buildTopology(nodes)
  const graph = useMemo(() => {
    const built = topologyGraph(topology)
    return layoutGraph({ nodes: built.nodes, edges: built.edges })
  }, [topology])

  function nodeById(id: string): NodeRecord | undefined {
    // The hub is the control row, whether the daemon reported it (id "host",
    // hostNode true) or it was synthesised. Matching on hostNode rather than on
    // the synthetic id is what keeps the two cases one code path.
    if (id === topology.control.id) return topology.control
    return nodes.find((node) => node.id === id)
  }
  // The node the diagram has selected, resolved once so the detail card
  // and the highlight cannot disagree.
  const selectedNodeDetail = selected === null ? null : nodeById(selected) ?? null

  // What the shared renderer needs per box. Kept here rather than in the
  // renderer so the renderer stays unaware of what a node is.
  function metaFor(id: string): GraphNodeMeta {
    const node = nodeById(id)
    if (node === undefined) return { label: id, kind: 'node' }
    // The hub, whichever way it arrived: the daemon reports it as a row with
    // hostNode true, and a daemon that does not gets the synthesised one.
    if (node.hostNode === true) {
      return { label: text.controlNode, kind: 'control', title: text.controlNodeTitle }
    }
    return {
      label: node.id,
      kind: 'node',
      title: `${node.kind} · ${node.platform} · ${statusLabel(node.status)}`,
      // A node that is not online is drawn muted, so the diagram reads as
      // a health check rather than an inventory.
      activated: node.status === 'online',
      issue: node.status === 'failed' ? 'missing' : null,
    }
  }

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
        {/* Two ways to read one fleet. The diagram is the default because
           it is the only one that shows the Box as the hub; the list stays
           for the detail a box cannot carry -- capabilities, timestamps,
           and a remove button per row. */}
        <div className="node-view-switch" role="group" aria-label={text.nodeViewLabel}>
          <Button
            variant={view === 'graph' ? 'primary' : 'ghost'}
            size="sm"
            onClick={() => setView('graph')}
          >
            {text.nodeViewGraph}
          </Button>
          <Button
            variant={view === 'list' ? 'primary' : 'ghost'}
            size="sm"
            onClick={() => setView('list')}
          >
            {text.nodeViewList}
          </Button>
        </div>
      </div>
      <p className="section-note">{text.nodesNote}</p>
      {/* Says the status is live, because the alternative is a page that
         looks live and is not. The daemon broadcasts every transition, so
         the claim is earned. */}
      <p className="node-live">
        <span className="node-live-dot" aria-hidden="true" />
        {text.nodeLive}
        <span className="node-live-note">{text.nodeLiveNote}</span>
      </p>

      {/* The fleet as a topology. The Box is the hub and every node hangs
          off it, which is the shape the protocol actually has: a helper
          opens one outbound connection to the control plane and nothing
          else, so there is no edge between two helpers. */}
      {view === 'graph' && (
        <div className="node-fleet">
          <PluginGraphView
            layout={graph}
            meta={metaFor}
            selected={selected}
            onSelect={setSelected}
            showEdgeLabels={topology.nodes.length <= 11}
            matches={null}
            layerLabel={() => ''}
            onToggleLayer={() => {}}
            canvasLabel={text.nodeGraphLabel}
            emptyLabel={text.noNodes}
          />
          {/* The selected node, beside the diagram. A box carries a name and
              a colour but not a capability list, and the detail is why the
              diagram is not the whole page. */}
          {selectedNodeDetail !== null && (
            <Card className="node-detail">
              <Stack gap={4}>
                <div>
                  <p className="eyebrow">NODE</p>
                  <h2>{selectedNodeDetail.id}</h2>
                </div>
                <p className="node-meta">
                  {selectedNodeDetail.kind} · {selectedNodeDetail.platform} · {statusLabel(selectedNodeDetail.status)}
                </p>
                {selectedNodeDetail.capabilities.length > 0 && (
                  <p className="node-meta">{selectedNodeDetail.capabilities.join(', ')}</p>
                )}
                {/* The Box never heartbeats, so its lastSeen is the enrol time
                    and would sit unchanged forever. Calling that "last seen"
                    is a lie about the one row that matters most. */}
                {selectedNodeDetail.hostNode !== true &&
                  selectedNodeDetail.lastSeen !== null && (
                  <p className="node-meta">
                    {text.nodeLastSeen} {new Date(selectedNodeDetail.lastSeen * 1000).toLocaleString()}
                  </p>
                )}
                {/* hostNode, not the synthetic id: the daemon reports the Box
                    as a row with hostNode true and its own id, so an id
                    comparison would miss it and offer a remove on a row the
                    daemon always refuses. */}
                {selectedNodeDetail.hostNode !== true && (
                  <Button
                    variant="ghost"
                    size="sm"
                    aria-label={text.nodeRemove}
                    title={text.nodeRemove}
                    disabled={removingId === selectedNodeDetail.id}
                    onClick={() => {
                      if (window.confirm(text.nodeRemoveConfirm(selectedNodeDetail.id))) onRemove(selectedNodeDetail.id)
                    }}
                  >
                    {text.nodeRemove}
                  </Button>
                )}
              </Stack>
            </Card>
          )}
        </div>
      )}

      {view === 'list' && (
      <div className="version-list node-list">
        {nodes.map((node) => (
          <section key={node.id} className="version-row">
            <div className="node-summary">
              <strong>{node.id}</strong>
              <p className="node-meta">
                {node.kind} · {node.platform} · {statusLabel(node.status)}
                {/* The Box never heartbeats; its timestamp is the enrol time. */}
                {node.hostNode !== true &&
                  node.lastSeen !== null &&
                  ` · ${text.nodeLastSeen} ${new Date(node.lastSeen * 1000).toLocaleString()}`}
              </p>
              {node.capabilities.length > 0 && <p className="node-meta">{node.capabilities.join(', ')}</p>}
            </div>
            <div className="node-actions">
              {/* The Box cannot be deregistered -- the daemon refuses it --
                  so the row does not offer the action. A confirm dialog
                  that always ends in an error is worse than no button. */}
              {node.hostNode === true ? null : (
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
              )}
           </div>
          </section>
        ))}
        {/* "No nodes" means no nodes besides the Box. The Box is always in
            the list now, so counting every row would make this note dead. */}
        {!loading && nodes.filter((node) => node.hostNode !== true).length === 0 && (
          <p className="empty-note">{text.noNodes}</p>
        )}
      </div>
      )}

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
