// Topology of the node fleet, as a layered DAG the shared graph layout can
// draw.
//
// The shape is a star, not a tree: the Box is the only node that initiates a
// connection, so every other node hangs off it. That is a fact about the
// protocol rather than a drawing choice -- a helper opens an outbound
// connection to the control plane and nothing else, so there is no edge
// between two helpers and no edge from a helper to the Box.
//
// Which is why the layout is asked for one layer of dependents rather than a
// depth: with a star, longest-path layering would put the Box at layer 0 and
// every node at layer 1, and the diagram would read as a crowd rather than as
// a hub. The Box is drawn as the hub because it is one.

import type { LayoutEdge } from '../plugin-graph/layout'
import type { NodeRecord } from '../../shared/types/domain'

/** The synthetic id of the Box itself. Not a node id, so it cannot collide. */
export const CONTROL_NODE_ID = '__control__'

export type NodeTopology = {
  /** The Box, first. Never absent -- a fleet without a control plane is not
   *  a state this UI can be in. */
  control: NodeRecord
  /** Every registered node, host node included, in registry order. */
  nodes: NodeRecord[]
};

/**
 * Build the topology from the registry.
 *
 * The host node is synthesised when the daemon did not send one, so a fleet
 * whose daemon predates the host row still draws a hub. Its status is always
 * `online`: it is this process, and a control plane that cannot reach itself
 * is not a control plane.
 */
export function buildTopology(nodes: NodeRecord[]): NodeTopology {
  // The daemon reports the Box as a row with hostNode: true. That row IS the
  // hub -- so it is lifted out rather than synthesised, and the diagram shows
  // one hub, not "This Box -> host".
  //
  // A daemon that reports no host row gets a synthesised one, because a fleet
  // diagram without its hub is a list with extra steps. Its id is the
  // synthetic one so it can never collide with a real node id.
  const reported = nodes.find((node) => node.hostNode === true)
  const control: NodeRecord =
    reported ?? {
      id: CONTROL_NODE_ID,
      kind: 'control',
      platform: 'this-host',
      status: 'online',
      capabilities: [],
      controlUrl: null,
      sessionEpoch: 0,
      enrolledAt: null,
      lastSeen: null,
      helperVersion: null,
      backendRef: null,
      hostNode: true,
    };
  // Every node that is not the hub. The daemon's own host row is excluded
  // because it is the control, drawn as the control rather than as a peer.
  const fleet = nodes.filter((node) => node.hostNode !== true)
  return { control, nodes: fleet }
}

/**
 * The graph input for the shared layout: one edge per node, all from the Box.
 *
 * A node that is registering or failed is still drawn -- an absent node is
 * indistinguishable from one that was never enrolled, and the whole point of
 * the diagram is to show what is wrong at a glance.
 */
export function topologyGraph(topology: NodeTopology): {
  nodes: string[]
  edges: LayoutEdge[]
} {
  const nodes = [topology.control.id, ...topology.nodes.map((node) => node.id)];
  const edges = topology.nodes.map((node) => ({
    from: topology.control.id,
    to: node.id,
    // The edge label is the node's kind, because that is what distinguishes
    // one line from another once the boxes are the same colour.
    label: node.kind,
    title: `${node.kind} · ${node.platform}`,
  }));
  return { nodes, edges };
}
