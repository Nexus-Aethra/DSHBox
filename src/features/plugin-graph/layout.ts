// Layered DAG layout for the plugin dependency graph.
//
// There is no graph library in this project, so the layout is hand-rolled. It is
// the classic three-step pipeline, trimmed to what a dependency diagram needs:
//
//   1. layer assignment    — longest path from the sources, so every edge points
//                            forward and dependencies read left to right;
//   2. ordering            — barycenter sweeps, keeping whichever sweep produced
//                            the fewest edge crossings;
//   3. coordinates         — fixed-size boxes in a grid, each column centred.
//
// The result is a plain data structure, so the renderer stays a pure function of
// its input and this file can be reasoned about without a DOM.
//
// Cycles are tolerated rather than rejected: cordis leaves a cyclic group
// silently pending, and a diagram that refuses to draw it is less useful than one
// that draws it and marks the offending edges as back edges.

export type LayoutEdge = {
  from: string
  to: string
  // Optional caption drawn on the edge, and hover text for it. They live here so
  // the renderer can draw an edge without knowing what the graph means.
  label?: string
  title?: string
}

export type LayoutInput = {
  nodes: string[]
  edges: LayoutEdge[]
  // Preferred within-layer order, typically the daemon's topological order. Nodes
  // it does not mention keep their relative input order after those it does.
  order?: string[]
}

export type PlacedNode = {
  id: string
  layer: number
  // Position of the box's top-left corner, in layout units.
  x: number
  y: number
}

export type PlacedEdge = LayoutEdge & {
  // True when the edge points backwards or sideways in the layer assignment,
  // which means it is part of (or runs into) a cycle.
  back: boolean
}

export type Layout = {
  nodes: PlacedNode[]
  edges: PlacedEdge[]
  width: number
  height: number
  layers: number
}

export const NODE_WIDTH = 176
export const NODE_HEIGHT = 34
const COLUMN_GAP = 48
const ROW_GAP = 8

// A layer is drawn as one band, but a bushy graph — many plugins, only a few
// dependency levels — puts most of its nodes in one layer, and a single band that
// large cannot be fitted legibly no matter how the canvas is sized. Layers
// therefore wrap: a layer taller than the row cap continues in the next band.
// Edges still run strictly forwards, because every band of a layer is placed
// before any band of the next one.
const COLUMN_STRIDE = NODE_WIDTH + COLUMN_GAP
const ROW_STRIDE = NODE_HEIGHT + ROW_GAP
// Roughly the graph stage in the panel, which is what the diagram has to fit.
const REFERENCE_WIDTH = 1000
const REFERENCE_HEIGHT = 620
// Candidate band sizes. Too small and the graph becomes a ribbon, too large and a
// single band cannot be fitted; the best candidate is picked by measurement.
const MIN_ROWS = 6
const MAX_ROWS = 18
// Stacked at 1:1 this many bands still fit the reference canvas height, so a
// diagram that exceeds it is not made legible by adding more bands — it needs to
// widen instead. A long dependency chain is the case that matters: each of its
// layers holds one plugin, so wrapping alone would leave one band per layer.
const MAX_BANDS = 14

/**
 * Cut the layer sequence into bands of at most `cap` nodes, keeping each layer's
 * nodes contiguous. Past MAX_BANDS consecutive small bands are then coalesced, so
 * a graph that is deep but sparse spends its canvas on columns rather than on
 * empty space. Merging never splits a band, so layer order is preserved.
 */
function wrapLayers(layers: string[][], cap: number): string[][] {
  const bands: string[][] = []
  for (const layer of layers) {
    for (let start = 0; start < layer.length; start += cap) {
      bands.push(layer.slice(start, start + cap))
    }
  }
  if (bands.length <= MAX_BANDS) return bands

  const merged: string[][] = []
  let current: string[] = []
  for (const band of bands) {
    if (current.length > 0 && current.length + band.length > cap) {
      merged.push(current)
      current = []
    }
    current.push(...band)
  }
  if (current.length > 0) merged.push(current)
  return merged
}

type Placement = {
  bands: string[][]
  horizontal: boolean
  width: number
  height: number
  fit: number
  area: number
}

/**
 * Size a band sequence laid out along one axis. Bands advance along the axis and
 * the nodes inside a band spread across it, so the two orientations are transposes
 * of each other and can be measured without placing anything.
 */
function measure(bands: string[][], horizontal: boolean): { width: number; height: number } {
  const widest = bands.reduce((most, band) => Math.max(most, band.length), 0)
  const alongStride = horizontal ? COLUMN_STRIDE : ROW_STRIDE
  const alongGap = horizontal ? COLUMN_GAP : ROW_GAP
  const crossStride = horizontal ? ROW_STRIDE : COLUMN_STRIDE
  const crossGap = horizontal ? ROW_GAP : COLUMN_GAP
  const along = bands.length * alongStride - alongGap
  const cross = widest * crossStride - crossGap
  return horizontal ? { width: along, height: cross } : { width: cross, height: along }
}

function placement(bands: string[][], horizontal: boolean): Placement {
  const { width, height } = measure(bands, horizontal)
  return {
    bands,
    horizontal,
    width,
    height,
    fit: Math.min(1, REFERENCE_WIDTH / width, REFERENCE_HEIGHT / height),
    area: width * height,
  }
}

/**
 * The scale a geometry is drawn at is limited by the canvas edge it overflows, so
 * the geometry that fits best wins. Ties go to the tighter packing, which draws at
 * the larger scale, and then to the conventional left-to-right reading direction.
 */
function isBetter(candidate: Placement, current: Placement): boolean {
  if (candidate.fit > current.fit + 1e-9) return true
  if (candidate.fit < current.fit - 1e-9) return false
  if (candidate.area < current.area - 1) return true
  if (candidate.area > current.area + 1) return false
  return candidate.horizontal && !current.horizontal
}

/**
 * Both shapes a plugin graph comes in want opposite orientations. A bushy graph
 * (many plugins, few dependency levels) has one or two enormous layers; a deep one
 * (a long dependency chain) has many layers holding almost nothing. So every
 * candidate band size is measured in both orientations and the best one wins —
 * picking on layer count or aspect alone leaves one of the two shapes overflowing.
 */
function chooseLayout(layers: string[][]): Placement {
  let best: Placement | null = null
  for (let cap = MIN_ROWS; cap <= MAX_ROWS; cap += 1) {
    const bands = wrapLayers(layers, cap)
    for (const horizontal of [true, false]) {
      const candidate = placement(bands, horizontal)
      if (best === null || isBetter(candidate, best)) best = candidate
    }
  }
  return best!
}

/** Longest label the node box can show at its font size before it overflows. */
export const MAX_LABEL_CHARS = 22

function uniqueEdges(edges: LayoutEdge[]): LayoutEdge[] {
  const seen = new Map<string, LayoutEdge>()
  for (const edge of edges) {
    if (edge.from === edge.to) continue
    const key = `${edge.from}\u0000${edge.to}`
    const existing = seen.get(key)
    if (existing === undefined) {
      seen.set(key, edge)
      continue
    }
    // Collapse a repeated pair into one line, combining what each carried so a
    // second relation is not silently dropped.
    if (edge.label !== undefined && edge.label !== existing.label) {
      existing.label = existing.label === undefined ? edge.label : `${existing.label}, ${edge.label}`
    }
    if (edge.title !== undefined && edge.title !== existing.title) {
      existing.title = existing.title === undefined ? edge.title : `${existing.title}; ${edge.title}`
    }
  }
  return [...seen.values()]
}

/**
 * Longest-path layering, tolerant of cycles.
 *
 * Kahn's algorithm assigns a layer to every node that is reachable in
 * topological order. Whatever it cannot reach sits on a cycle; those nodes are
 * then placed one layer past their deepest already-placed predecessor, which
 * keeps the drawing readable without pretending the cycle is acyclic.
 */
function assignLayers(nodes: string[], edges: LayoutEdge[]): Map<string, number> {
  const predecessors = new Map<string, string[]>()
  const successors = new Map<string, string[]>()
  const inDegree = new Map<string, number>()
  for (const node of nodes) {
    predecessors.set(node, [])
    successors.set(node, [])
    inDegree.set(node, 0)
  }
  for (const edge of edges) {
    if (!predecessors.has(edge.to) || !successors.has(edge.from)) continue
    successors.get(edge.from)!.push(edge.to)
    predecessors.get(edge.to)!.push(edge.from)
    inDegree.set(edge.to, (inDegree.get(edge.to) ?? 0) + 1)
  }

  const layer = new Map<string, number>()
  const remaining = new Map(inDegree)
  const queue = nodes.filter((node) => remaining.get(node) === 0)
  for (const node of queue) layer.set(node, 0)

  while (queue.length > 0) {
    const node = queue.shift()!
    for (const next of successors.get(node) ?? []) {
      layer.set(next, Math.max(layer.get(next) ?? 0, (layer.get(node) ?? 0) + 1))
      const left = (remaining.get(next) ?? 0) - 1
      remaining.set(next, left)
      if (left === 0) queue.push(next)
    }
  }

  // Everything left is on a cycle. Place each pass by pass so a chain hanging off
  // a cycle also ends up below it.
  let pending = nodes.filter((node) => !layer.has(node))
  for (let pass = 0; pending.length > 0 && pass < nodes.length; pass += 1) {
    const stillPending: string[] = []
    for (const node of pending) {
      const placed = (predecessors.get(node) ?? []).filter((parent) => layer.has(parent))
      if (placed.length === 0) {
        stillPending.push(node)
        continue
      }
      const deepest = Math.max(...placed.map((parent) => layer.get(parent)!))
      layer.set(node, deepest + 1)
    }
    // A pending node with no placed predecessor is itself the start of a cycle.
    if (stillPending.length === pending.length) {
      for (const node of stillPending) layer.set(node, 0)
      break
    }
    pending = stillPending
  }
  return layer
}

function groupByLayer(nodes: string[], layer: Map<string, number>): string[][] {
  const maxLayer = nodes.reduce((deepest, node) => Math.max(deepest, layer.get(node) ?? 0), 0)
  const layers: string[][] = Array.from({ length: maxLayer + 1 }, () => [])
  for (const node of nodes) layers[layer.get(node) ?? 0].push(node)
  return layers
}

/** Count edge crossings between every adjacent pair of layers. */
function countCrossings(layers: string[][], edges: LayoutEdge[]): number {
  const position = new Map<string, number>()
  layers.forEach((column, index) =>
    column.forEach((node, offset) => position.set(`${index}\u0000${node}`, offset)),
  )
  let crossings = 0
  for (let index = 0; index + 1 < layers.length; index += 1) {
    const lower = new Set(layers[index + 1])
    // Only edges spanning exactly these two layers are drawn between them.
    const pairs: Array<[number, number]> = []
    for (const edge of edges) {
      const from = position.get(`${index}\u0000${edge.from}`)
      if (from === undefined || !lower.has(edge.to)) continue
      pairs.push([from, position.get(`${index + 1}\u0000${edge.to}`)!])
    }
    pairs.sort((left, right) => left[0] - right[0] || left[1] - right[1])
    for (let i = 0; i < pairs.length; i += 1) {
      for (let j = i + 1; j < pairs.length; j += 1) {
        if (pairs[i][0] < pairs[j][0] && pairs[i][1] > pairs[j][1]) crossings += 1
      }
    }
  }
  return crossings
}

/**
 * Barycenter sweeps. Each pass orders a layer by the average position of each
 * node's neighbours in the adjacent layer, which is the standard cheap heuristic
 * for pulling connected nodes into vertical alignment. The best-scoring
 * arrangement is kept rather than the last one, because a sweep can also make
 * things worse.
 */
function orderLayers(
  layers: string[][],
  edges: LayoutEdge[],
  predecessors: Map<string, string[]>,
  successors: Map<string, string[]>,
): string[][] {
  const sweep = (forward: boolean): void => {
    const indices = forward
      ? layers.map((_, index) => index).slice(1)
      : layers.map((_, index) => index).slice(0, -1).reverse()
    for (const index of indices) {
      const adjacent = forward ? layers[index - 1] : layers[index + 1]
      const neighbourIndex = new Map(adjacent.map((node, offset) => [node, offset]))
      const neighbourOf = forward ? predecessors : successors
      const barycenter = new Map<string, number>()
      for (const node of layers[index]) {
        const neighbours = (neighbourOf.get(node) ?? [])
          .map((neighbour) => neighbourIndex.get(neighbour))
          .filter((value): value is number => value !== undefined)
        barycenter.set(
          node,
          neighbours.length === 0
            ? Number.MAX_SAFE_INTEGER
            : neighbours.reduce((sum, value) => sum + value, 0) / neighbours.length,
        )
      }
      // Sort a copy so the comparator can still read the current positions, which
      // keeps nodes with equal barycenters in their existing order.
      const current = layers[index]
      const rank = new Map(current.map((node, offset) => [node, offset]))
      layers[index] = [...current].sort((left, right) => {
        const delta = barycenter.get(left)! - barycenter.get(right)!
        if (delta !== 0) return delta
        return rank.get(left)! - rank.get(right)!
      })
    }
  }

  let best = layers.map((column) => [...column])
  let bestCrossings = countCrossings(best, edges)
  for (let pass = 0; pass < 4 && bestCrossings > 0; pass += 1) {
    sweep(pass % 2 === 0)
    const crossings = countCrossings(layers, edges)
    if (crossings < bestCrossings) {
      bestCrossings = crossings
      best = layers.map((column) => [...column])
    }
  }
  return best
}

export function layoutGraph(input: LayoutInput): Layout {
  const nodes = [...input.nodes]
  const edges = uniqueEdges(input.edges)
  if (nodes.length === 0) {
    return { nodes: [], edges: [], width: 0, height: 0, layers: 0 }
  }

  const layer = assignLayers(nodes, edges)
  const layers = groupByLayer(nodes, layer)

  // Seed each layer with the caller's preferred order before the sweeps run.
  if (input.order && input.order.length > 0) {
    const rank = new Map(input.order.map((id, index) => [id, index]))
    for (const column of layers) {
      column.sort((left, right) => {
        const leftRank = rank.get(left) ?? Number.MAX_SAFE_INTEGER
        const rightRank = rank.get(right) ?? Number.MAX_SAFE_INTEGER
        if (leftRank !== rightRank) return leftRank - rightRank
        return left.localeCompare(right)
      })
    }
  }

  const predecessors = new Map<string, string[]>()
  const successors = new Map<string, string[]>()
  for (const node of nodes) {
    predecessors.set(node, [])
    successors.set(node, [])
  }
  for (const edge of edges) {
    if (!predecessors.has(edge.to) || !successors.has(edge.from)) continue
    predecessors.get(edge.to)!.push(edge.from)
    successors.get(edge.from)!.push(edge.to)
  }

  const ordered = orderLayers(layers, edges, predecessors, successors)
  const { bands, horizontal, width, height } = chooseLayout(ordered)

  // Within its band every node is centred along the cross axis, so the diagram
  // reads as a spine rather than a staircase.
  const placed: PlacedNode[] = []
  bands.forEach((band, bandIndex) => {
    const crossStride = horizontal ? ROW_STRIDE : COLUMN_STRIDE
    const crossGap = horizontal ? ROW_GAP : COLUMN_GAP
    const bandSpan = band.length * crossStride - crossGap
    const crossOffset = ((horizontal ? height : width) - bandSpan) / 2
    band.forEach((id, index) => {
      const along = bandIndex * (horizontal ? COLUMN_STRIDE : ROW_STRIDE)
      const across = crossOffset + index * crossStride
      placed.push({
        id,
        layer: layer.get(id) ?? 0,
        x: horizontal ? along : across,
        y: horizontal ? across : along,
      })
    })
  })

  const placedEdges: PlacedEdge[] = edges.map((edge) => ({
    ...edge,
    back: (layer.get(edge.to) ?? 0) <= (layer.get(edge.from) ?? 0),
  }))

  return {
    nodes: placed,
    edges: placedEdges,
    width,
    height,
    layers: ordered.length,
  }
}
