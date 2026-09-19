import type { Layout, PlacedEdge } from './layout'
import { NODE_HEIGHT, NODE_WIDTH } from './layout'

/** What the renderer needs to know about a node beyond its position. */
export type GraphNodeMeta = {
  label: string
  /** Full name for the tooltip; the box label is elided to fit. */
  title?: string
  kind: 'plugin' | 'service'
  // A plugin outside the profile's activation closure is installed but never
  // loaded, which is why it is drawn muted rather than as a normal node.
  activated?: boolean
  issue?: 'missing' | 'inactive-provider' | 'cycle' | null
}

type Props = {
  layout: Layout
  meta: (id: string) => GraphNodeMeta
  selected: string | null
  onSelect: (id: string | null) => void
  // Service names on every edge; only worth it while the diagram is small.
  showEdgeLabels: boolean
  // Nodes the search box matched, outlined rather than isolated so the hit keeps
  // the surrounding graph as context.
  matches: Set<string> | null
  // Accessible name for the diagram, and the message shown when it is empty.
  canvasLabel: string
  emptyLabel: string
}

const PADDING = 16
const ARROW_GAP = 7

type Box = { x: number; y: number }

// The layout may run left to right or top to bottom depending on the graph's
// shape, so the anchor sides are derived from where the two boxes actually sit
// rather than assumed. Sharing one rule keeps the renderer unaware of which
// orientation produced the coordinates.
function anchors(edge: PlacedEdge, from: Box, to: Box) {
  const dx = to.x - from.x
  const dy = to.y - from.y
  const mostlyHorizontal = Math.abs(dx) >= Math.abs(dy)
  if (mostlyHorizontal) {
    const fromX = dx >= 0 ? from.x + NODE_WIDTH : from.x
    const toX = dx >= 0 ? to.x : to.x + NODE_WIDTH
    const direction = toX >= fromX ? 1 : -1
    return {
      startX: fromX,
      startY: from.y + NODE_HEIGHT / 2,
      endX: toX - direction * ARROW_GAP,
      endY: to.y + NODE_HEIGHT / 2,
      horizontal: true,
      direction,
    }
  }
  const fromY = dy >= 0 ? from.y + NODE_HEIGHT : from.y
  const toY = dy >= 0 ? to.y : to.y + NODE_HEIGHT
  const direction = toY >= fromY ? 1 : -1
  return {
    startX: from.x + NODE_WIDTH / 2,
    startY: fromY,
    endX: to.x + NODE_WIDTH / 2,
    endY: toY - direction * ARROW_GAP,
    horizontal: false,
    direction,
  }
}

function curve(edge: PlacedEdge, from: Box, to: Box): string {
  const a = anchors(edge, from, to)
  const bow = Math.max(28, (a.horizontal ? Math.abs(a.endX - a.startX) : Math.abs(a.endY - a.startY)) / 2)
  return a.horizontal
    ? `M ${a.startX} ${a.startY} C ${a.startX + a.direction * bow} ${a.startY}, ${a.endX - a.direction * bow} ${a.endY}, ${a.endX} ${a.endY}`
    : `M ${a.startX} ${a.startY} C ${a.startX} ${a.startY + a.direction * bow}, ${a.endX} ${a.endY - a.direction * bow}, ${a.endX} ${a.endY}`
}

function edgeMidpoint(edge: PlacedEdge, from: Box, to: Box): { x: number; y: number } {
  const a = anchors(edge, from, to)
  return {
    x: (a.startX + a.endX) / 2,
    y: (a.startY + a.endY) / 2 - (a.horizontal ? 4 : 0),
  }
}

export function PluginGraphView({ layout, meta, selected, onSelect, showEdgeLabels, matches, canvasLabel, emptyLabel }: Props) {
  if (layout.nodes.length === 0) {
    return <p className="plugin-graph-empty">{emptyLabel}</p>
  }

  const positions = new Map(layout.nodes.map((node) => [node.id, node]))
  const width = layout.width + PADDING * 2
  const height = layout.height + PADDING * 2

  // Selecting a node narrows the diagram to that node's neighbourhood. In a graph
  // this size — the web profile activates ~160 service participants — unrelated
  // edges stay in the picture otherwise, and the shape of one plugin's
  // dependencies is impossible to trace through the haze.
  const neighbourhood = new Set<string>()
  if (selected !== null) {
    neighbourhood.add(selected)
    for (const edge of layout.edges) {
      if (edge.from === selected) neighbourhood.add(edge.to)
      else if (edge.to === selected) neighbourhood.add(edge.from)
    }
  }
  const focused = selected !== null

  return (
    <div className="plugin-graph-canvas">
      <svg
        viewBox={`${-PADDING} ${-PADDING} ${width} ${height}`}
        width={width}
        height={height}
        // A group, not an image: the nodes inside are focusable buttons, and
        // `role="img"` would tell assistive technology to ignore them.
        role="group"
        aria-label={canvasLabel}
      >
        <defs>
          <marker id="plugin-graph-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="plugin-graph-arrow-head" />
          </marker>
          <marker id="plugin-graph-arrow-back" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="plugin-graph-arrow-head back" />
          </marker>
          {/* An arrowhead carries its own fill, so a highlighted edge with a grey
              head pointed nowhere while the line under it was blue. */}
          <marker id="plugin-graph-arrow-active" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="plugin-graph-arrow-head active" />
          </marker>
        </defs>

        <g className="plugin-graph-edges">
          {layout.edges.map((edge) => {
            const from = positions.get(edge.from)
            const to = positions.get(edge.to)
            if (!from || !to) return null
            const touched = selected === edge.from || selected === edge.to
            const midpoint = edgeMidpoint(edge, from, to)
            const label = (showEdgeLabels || touched) && edge.label
            const classes = ['edge', touched ? 'touched' : '', focused && !touched ? 'muted' : '']
              .filter(Boolean)
              .join(' ')
            return (
              <g key={`${edge.from}->${edge.to}`} className={classes}>
                {edge.title !== undefined && <title>{edge.title}</title>}
                <path
                  d={curve(edge, from, to)}
                  className={`plugin-graph-edge${edge.back ? ' back' : ''}`}
                  markerEnd={
                    edge.back
                      ? 'url(#plugin-graph-arrow-back)'
                      : touched
                        ? 'url(#plugin-graph-arrow-active)'
                        : 'url(#plugin-graph-arrow)'
                  }
                />
                {label ? (
                  <text className="plugin-graph-edge-label" x={midpoint.x} y={midpoint.y - 4} textAnchor="middle">
                    {edge.label}
                  </text>
                ) : null}
              </g>
            )
          })}
        </g>

        <g className="plugin-graph-nodes">
          {layout.nodes.map((node) => {
            const info = meta(node.id)
            const classes = ['plugin-graph-node', info.kind, info.activated === false ? 'inactive' : '', info.issue ?? '', selected === node.id ? 'selected' : '', focused && !neighbourhood.has(node.id) ? 'muted' : '', matches?.has(node.id) ? 'match' : '']
              .filter(Boolean)
              .join(' ')
            return (
              <g
                key={node.id}
                className={classes}
                transform={`translate(${node.x}, ${node.y})`}
                onClick={() => { onSelect(selected === node.id ? null : node.id) }}
                role="button"
                tabIndex={0}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault()
                    onSelect(selected === node.id ? null : node.id)
                  }
                }}
              >
                <title>{info.title ?? info.label}</title>
                <rect width={NODE_WIDTH} height={NODE_HEIGHT} rx={info.kind === 'service' ? NODE_HEIGHT / 2 : 9} />
                <text x={NODE_WIDTH / 2} y={NODE_HEIGHT / 2 + 4} textAnchor="middle">
                  {info.label}
                </text>
              </g>
            )
          })}
        </g>
      </svg>
    </div>
  )
}
