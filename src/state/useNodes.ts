import { useEffect, useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { EnrollReply, NodeRecord } from '../shared/types/domain'

/**
 * The one daemon frame this hook listens for.
 *
 * Narrow on purpose: the daemon broadcasts a dozen event names and this page
 * cares about exactly one. A wide union here would be a second copy of the
 * task event list, and the two would drift.
 */
type NodeEvent = { event: 'node_status'; payload: { id: string; status: string } }

/** Every status the daemon can put on the wire. The daemon spells them
 *  lower case and a test pins that, so this list is the wire contract rather
 *  than a local guess. */
const NODE_STATUSES: NodeRecord['status'][] = ['registering', 'online', 'offline', 'draining', 'failed']

/**
 * Node registry state, and the two actions the control plane may take on it.
 *
 * Loaded on entry, not on mount: like the container section, the Nodes page
 * asks for its own data when the user opens it, so the daemon is never queried
 * for a page nobody is looking at.
 *
 * The host node is part of `nodes`. It is built in, never heartbeats, and
 * cannot be deregistered, so the page must not offer it a remove button --
 * which is a reason to guard the action, not to hide the node. The fleet
 * diagram is built around it.
 */
export function useNodes(onError: (message: string | null) => void) {
  const [nodes, setNodes] = useState<NodeRecord[]>([])
  const [selectedNode, setSelectedNode] = useState<NodeRecord | null>(null)
  const [loading, setLoading] = useState(false)
  const [removingId, setRemovingId] = useState<string | null>(null)
  // The enrol reply, held only until the user dismisses it. It carries the
  // bootstrap seed, which exists in exactly one place after this -- so the
  // copy field is the last chance to keep it.
  const [enrolled, setEnrolled] = useState<EnrollReply | null>(null)
  const [enrolling, setEnrolling] = useState(false)
  // Set while the page is open, so the subscription below knows it is live.
  // The daemon broadcasts node_status on every transition -- registering,
  // online, offline, draining, failed -- so a node going dark reaches this
  // page without the user having to ask for it.
  const [live, setLive] = useState(false)

  async function loadNodes(): Promise<void> {
    setLoading(true)
    try {
      const all = await boxApi.listNodes()
      // The host node is kept. It used to be filtered out here, on the
      // grounds that it cannot be deregistered -- but that is a reason not
      // to offer it a remove button, not a reason to hide it. The fleet
      // diagram needs it: the Box is the hub every other node hangs off,
      // and a diagram without its hub is a list with extra steps.
      setNodes(all)
      onError(null)
    } catch (reason) {
      onError(String(reason))
    } finally {
      setLoading(false)
    }
  }

  // Subscribe while the page is open. The frame names the node and its new
  // status and nothing else, so the row is patched in place rather than the
  // whole list being re-fetched: a re-fetch on every heartbeat would be one
  // request per node per interval, for a list that is already correct except
  // for one field.
  useEffect(() => {
    if (!live) return
    const unlisten = boxApi.listenTask<NodeEvent>('daemon://event', (frame) => {
      if (frame.event !== 'node_status') return
      const { id, status } = frame.payload
      // The daemon pins the wire spelling and NodeStatus lists exactly those
      // five, so today the cast cannot fail. But a sixth daemon status would
      // otherwise render as undefined rather than surface, so the value is
      // checked against the known set and an unknown one leaves the row alone
      // -- which keeps the last real status on screen instead of a blank.
      if (!NODE_STATUSES.includes(status as NodeRecord['status'])) return
      setNodes((current) => current.map((node) => (node.id === id ? { ...node, status: status as NodeRecord['status'] } : node)))
    })
    return () => {
      void unlisten.then((off) => off())
    };
  }, [live])

  async function enroll(kind: string, platform: string, nodeId: string | null): Promise<void> {
    setEnrolling(true)
    try {
      const reply = await boxApi.enrollNode(kind, platform, nodeId)
      setEnrolled(reply)
      // A node that was refused -- one that already has a bootstrap key --
      // never produces a reply, so this reload only happens on success.
      await loadNodes()
      onError(null)
    } catch (reason) {
      onError(String(reason))
    } finally {
      setEnrolling(false)
    }
  }

  async function removeNode(id: string): Promise<void> {
    setRemovingId(id)
    try {
      await boxApi.deregisterNode(id)
      if (selectedNode?.id === id) setSelectedNode(null)
      await loadNodes()
      onError(null)
    } catch (reason) {
      onError(String(reason))
    } finally {
      setRemovingId(null)
    }
  }

  function dismissEnrolled(): void {
    setEnrolled(null)
  }

  return {
    nodes,
    selectedNode,
    setSelectedNode,
    loading,
    removingId,
    enrolled,
    enrolling,
    loadNodes,
    // Enter and leave are separate from loadNodes so the subscription can be
    // torn down when the page closes, without a second load on the way out.
    enter: () => { setLive(true); void loadNodes() },
    leave: () => setLive(false),
    enroll,
    removeNode,
    dismissEnrolled,
  }
}
