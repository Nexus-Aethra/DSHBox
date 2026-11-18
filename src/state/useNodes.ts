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

/**
 * Node registry state, and the two actions the control plane may take on it.
 *
 * Loaded on entry, not on mount: like the container section, the Nodes page
 * asks for its own data when the user opens it, so the daemon is never queried
 * for a page nobody is looking at.
 *
 * The host node is filtered out of `nodes` and exposed separately. It is
 * built in, never heartbeats, and cannot be deregistered, so offering it a
 * remove button would be offering an action that always fails.
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
      setNodes(all.filter((node) => !node.hostNode))
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
