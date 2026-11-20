/** What a node does in the fleet.
 *
 * Two roles, and a node can hold both. The Box itself always does: it parses
 * instructions and dispatches work (control), and it runs containers itself
 * (executor). A helper enrolled from a machine is an executor only.
 *
 * This is derived, not stored. `NodeRecord` carries capabilities -- the
 * actions a node can perform -- and the daemon grants `dsh-host` to the
 * control plane and `exec` to anything that runs work. So the role is a
 * reading of what a node can do rather than a field the wire carries, and a
 * new capability does not need a migration to be visible here.
 */
export type NodeRole = 'control' | 'executor'

/** The roles a node holds, control first so the order is stable. */
export function rolesOf(node: {
  capabilities: string[]
  hostNode: boolean
}): NodeRole[] {
  const roles: NodeRole[] = []
  // `dsh-host` is what the daemon writes on the host row and nowhere else, so
  // in every row that exists it means control. It is a reading of what the
  // daemon writes, not a guarantee it gives: an enrol accepts any capability
  // string, and `register` is what keeps this true by filtering it out of
  // anything a helper reports. `hostNode` is the same fact from the other
  // side, for the row the daemon wrote about itself -- either one means
  // control, and the redundancy is deliberate.
  if (node.capabilities.includes('dsh-host') || node.hostNode) roles.push('control')
  // `exec` is granted to anything that runs work, including the control
  // plane -- which is why the Box shows both and a helper shows one.
  if (node.capabilities.includes('exec')) roles.push('executor')
  return roles
}
