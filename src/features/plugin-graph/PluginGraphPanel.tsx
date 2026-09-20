import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { GraphSourceKind, PluginGraph } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Input } from '../../ui/Input'
import type { LayoutEdge } from './layout'
import { layoutGraph, MAX_LABEL_CHARS, NODE_HEIGHT, NODE_WIDTH } from './layout'
import type { GraphNodeMeta } from './PluginGraphView'
import { PluginGraphView } from './PluginGraphView'

/** Text keys, named to match the global `Text` so callers pass `text` directly. */
export type PluginGraphText = {
  pluginGraphOpen: string
  pluginGraphTitle: (name: string) => string
  pluginGraphLoading: string
  pluginGraphError: (message: string) => string
  pluginGraphEmpty: string
  pluginGraphCanvas: string
  pluginGraphViewPlugins: string
  pluginGraphViewServices: string
  pluginGraphSearch: string
  pluginGraphSearchHidden: (count: number) => string
  pluginGraphMatch: (count: number) => string
  pluginGraphShowIsolated: string
  pluginGraphShowInactive: string
  pluginGraphHelp: string
  pluginGraphHiddenIsolated: (count: number) => string
  pluginGraphHiddenInactive: (count: number) => string
  pluginGraphDirectionHint: string
  pluginGraphLegendPlugin: string
  pluginGraphLegendService: string
  pluginGraphInserts: string
  pluginGraphHalfClient: string
  pluginGraphLayerLabel: (layer: number, count: number) => string
  pluginGraphHalfHost: string
  pluginGraphHalvesBoth: string
  pluginGraphSplitHalves: string
  pluginGraphLegendInactive: string
  // Red marks three conditions; each names itself and carries its own count.
  pluginGraphLegendMissing: (count: number) => string
  pluginGraphLegendInactiveProvider: (count: number) => string
  pluginGraphLegendCycle: (count: number) => string
  pluginGraphMissingHint: string
  pluginGraphInactiveProviderHint: string
  pluginGraphCycleHint: string
  pluginGraphBackEdgeHint: string
  pluginGraphDetailCycle: string
  pluginGraphFit: string
  close: string
  pluginGraphDiagnostics: string
  pluginGraphNoIssues: string
  pluginGraphMissing: (count: number) => string
  pluginGraphInactive: (count: number) => string
  pluginGraphCycles: (count: number) => string
  pluginGraphSharedServices: (count: number) => string
  pluginGraphSharedService: (providers: string) => string
  pluginGraphSharedServiceHint: string
  pluginGraphParseNotes: (count: number) => string
  pluginGraphParseNotesHint: string
  pluginGraphOrder: string
  pluginGraphOrderHint: string
  pluginGraphOrderPending: string
  pluginGraphOrderUnavailable: string
  pluginGraphProvides: string
  pluginGraphRequires: string
  pluginGraphConsumedBy: string
  pluginGraphActivated: string
  pluginGraphInactiveNode: string
  pluginGraphSource: string
  pluginGraphNothing: string
  pluginGraphEdge: (from: string, to: string, service: string) => string
  pluginGraphProvidesEdge: (plugin: string, service: string) => string
  pluginGraphRequiresEdge: (plugin: string, service: string) => string
  pluginGraphBlocked: (plugin: string, service: string) => string
  pluginGraphBlockedInactive: (plugin: string, service: string) => string
}

type Props = {
  kind: GraphSourceKind
  id: string
  text: PluginGraphText
  onClose: () => void
}

type GraphView = 'plugins' | 'services'

// Past this many edges, naming every edge becomes unreadable, so labels are
// reserved for the selected node's edges instead.
const EDGE_LABEL_LIMIT = 48

// Fitting a wide graph to the stage can shrink labels past legibility, at which
// point the diagram stops answering the question it exists for. Below this the
// fit stops shrinking and the rest is reached by panning.
const MIN_READABLE_SCALE = 0.75
// The hard floor and ceiling on manual zoom. The floor is below the readable
// scale on purpose: zooming out past legibility is a legitimate overview, it is
// just not something "Fit" should decide for the reader.
const MIN_SCALE = 0.2
const MAX_SCALE = 2.5

const clampScale = (scale: number): number => Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale))
// A drag that merely nudges should still count as a click on the node under it.
const DRAG_THRESHOLD_PX = 3

// Core plugin names dominate the diagram and share one scope prefix; dropping it
// roughly halves the label width, which is what makes them readable at all. What
// is still too long for the box is elided, since SVG text does not wrap — the
// full name stays in the node tooltip and the detail panel.
function shortLabel(name: string, max = MAX_LABEL_CHARS): string {
  const short = name.startsWith('@deepseek-ai/') ? name.slice('@deepseek-ai/'.length) : name
  return short.length > max ? `${short.slice(0, max - 1)}…` : short
}

export function PluginGraphPanel({ kind, id, text, onClose }: Props) {
  const [graph, setGraph] = useState<PluginGraph | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [view, setView] = useState<GraphView>('plugins')
  const [showIsolated, setShowIsolated] = useState(false)
  const [showInactive, setShowInactive] = useState(false)
  const [selected, setSelected] = useState<string | null>(null)
  const [transform, setTransform] = useState({ scale: 1, x: 0, y: 0 })
  const [query, setQuery] = useState('')
  const [showHelp, setShowHelp] = useState(false)
  const [showParseNotes, setShowParseNotes] = useState(false)
  // Merged by default: a dual-face package is one plugin to a reader looking for
  // their plugins, and the two halves are there on request. The analysis never
  // merges — see `displayId`.
  const [splitHalves, setSplitHalves] = useState(false)
  const canvasRef = useRef<HTMLDivElement | null>(null)
  // A node a filter hides has no box to centre, so revealing it and centring it
  // cannot happen in the same pass. The reveal re-renders, and this carries the
  // request across to the layout that follows.
  const pendingFocus = useRef<string | null>(null)

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    boxApi.pluginDependencyGraph(kind, id)
      .then((next) => { if (!cancelled) setGraph(next) })
      .catch((reason: unknown) => { if (!cancelled) setError(String(reason)) })
      .finally(() => { if (!cancelled) setLoading(false) })
    return () => { cancelled = true }
  }, [kind, id])

  // Keyed by node id, not package name: a dual-face package is two nodes sharing
  // one name, so a name-keyed map would lose one of them.
  const byId = useMemo(() => {
    const map = new Map<string, PluginGraph['plugins'][number]>()
    for (const plugin of graph?.plugins ?? []) map.set(plugin.id, plugin)
    return map
  }, [graph])

  // A dual-face package's host and browser halves are two nodes in the graph and
  // one entry in the drawing, unless the reader asks for them apart. Edges are
  // remapped through this too, so a link to the browser half lands on the package
  // the reader sees.
  const halfOf = useMemo(() => {
    const map = new Map<string, string>()
    if (!splitHalves) {
      for (const plugin of graph?.plugins ?? []) {
        if (plugin.half === 'client' && plugin.id.endsWith('#client')) {
          map.set(plugin.id, plugin.id.slice(0, -'#client'.length))
        }
      }
    }
    return map
  }, [graph, splitHalves])
  const displayId = (id: string): string => halfOf.get(id) ?? id
  /** The nodes a display node stands for: one, or a dual-face package's two. */
  const halvesOf = (id: string): PluginGraph['plugins'] =>
    (graph?.plugins ?? []).filter((plugin) => displayId(plugin.id) === id)

  // The label a node box shows. A browser half carries a marker, because its
  // package name is shared with the host node and the two would otherwise be
  // indistinguishable in the drawing.
  const nodeLabel = (id: string): string => {
    const plugin = byId.get(id)
    const base = plugin?.name ?? id
    // Only a separated browser half needs the marker; merged, the box is the
    // package and its name says everything.
    if (plugin?.half !== 'client' || !splitHalves) return shortLabel(base)
    const marker = ` ·${text.pluginGraphHalfClient}`
    return `${shortLabel(base, MAX_LABEL_CHARS - marker.length)}${marker}`
  }

  /** Full name for tooltips and detail, where the marker is spelled out. */
  const fullName = (id: string): string => {
    const plugin = byId.get(id)
    if (plugin === undefined) return id
    return plugin.half === 'client' ? `${plugin.name} (${text.pluginGraphHalfClient})` : plugin.name
  }

  // Isolated plugins — no provides, no requires — say nothing about
  // dependencies, so they are dropped unless asked for. So are plugins the
  // profile never loads: their edges describe wiring that cannot happen at
  // runtime, and in the real container they are the bulk of the hairball (one
  // test-support package providing `remote` alone accounted for 127 of 871
  // edges). Both counts stay visible so the omission is never silent.
  //
  // Anything named by a link, a diagnostic or a cycle counts as a participant:
  // a plugin with no edges of its own can still be the reason a diagnostic exists.
  const participants = useMemo(() => {
    const set = new Set<string>()
    for (const link of graph?.links ?? []) {
      set.add(link.from)
      set.add(link.to)
    }
    for (const edge of [...(graph?.missing ?? []), ...(graph?.inactiveProviders ?? [])]) {
      set.add(edge.plugin)
    }
    for (const cycle of graph?.cycles ?? []) for (const name of cycle) set.add(name)
    // A bundle and its contents. `@nexus-aethra/dshell-bundle` reads nothing —
    // it is a manifest, a patch file and a compiled `lib/` — so without this the
    // one package the reader installed is an empty node, which the default view
    // then drops as isolated along with the eleven siblings it mounts.
    const idsByName = new Map<string, string[]>()
    for (const plugin of graph?.plugins ?? []) {
      idsByName.set(plugin.name, [...(idsByName.get(plugin.name) ?? []), plugin.id])
    }
    // A boxfile's plugins are real nodes of the preview even though nothing in the
    // template tree declares them, so they must not be dropped as isolated.
    for (const spec of graph?.recipePlugins ?? []) set.add(spec)
    for (const plugin of graph?.plugins ?? []) {
      const inserts = plugin.inserts ?? []
      if (inserts.length === 0) continue
      set.add(plugin.id)
      for (const name of inserts) for (const id of idsByName.get(name) ?? []) set.add(id)
    }
    return set
  }, [graph])

  const visible = useMemo(() => {
    const plugins = graph?.plugins ?? []
    const shown = plugins.filter((plugin) => {
      if (!showInactive && !plugin.activated) return false
      if (!showIsolated && !participants.has(plugin.id)) return false
      return true
    })
    // The two counts are disjoint: a hidden plugin is either outside the graph
    // entirely or in it but not loaded.
    const hiddenIsolated = showIsolated
      ? 0
      : plugins.filter((plugin) => !participants.has(plugin.id)).length
    const hiddenInactive = showInactive
      ? 0
      : plugins.filter((plugin) => participants.has(plugin.id) && !plugin.activated).length
    return { plugins: shown, hiddenIsolated, hiddenInactive }
  }, [graph, showIsolated, showInactive, participants])

  const serviceProviders = useMemo(() => {
    const map = new Map<string, string[]>()
    for (const edge of graph?.provides ?? []) {
      map.set(edge.service, [...(map.get(edge.service) ?? []), edge.plugin])
    }
    return map
  }, [graph])

  const serviceConsumers = useMemo(() => {
    const map = new Map<string, string[]>()
    for (const edge of graph?.requires ?? []) {
      map.set(edge.service, [...(map.get(edge.service) ?? []), edge.plugin])
    }
    return map
  }, [graph])

  const missingServices = useMemo(() => new Set((graph?.missing ?? []).map((edge) => edge.service)), [graph])
  const inactiveServices = useMemo(
    () => new Set((graph?.inactiveProviders ?? []).map((edge) => edge.service)),
    [graph],
  )
  // A plugin hangs when something it waits for cannot arrive, so the mark belongs
  // on the consumer rather than on the service it asked for. The three mark kinds
  // are counted separately because the legend names them separately; a node is
  // painted with whichever comes first here, and a node in a cycle is a cycle
  // member even when it also waits for something missing.
  const marks = useMemo(() => {
    const missing = new Set<string>()
    for (const edge of graph?.missing ?? []) missing.add(edge.plugin)
    const inactiveProvider = new Set<string>()
    for (const edge of graph?.inactiveProviders ?? []) inactiveProvider.add(edge.plugin)
    const cycle = new Set<string>()
    for (const group of graph?.cycles ?? []) for (const name of group) cycle.add(name)
    const count = (set: Set<string>): number => [...set].filter((name) => !cycle.has(name)).length
    return {
      cycle,
      missing,
      inactiveProvider,
      cycleCount: cycle.size,
      missingCount: count(missing),
      inactiveProviderCount: count(inactiveProvider),
    }
  }, [graph])
  const cycleMembers = marks.cycle
  // `pnpm dev` serves this panel against a daemon the reader started themselves,
  // so the two halves can be different builds; `sharedServices` is absent from a
  // daemon that predates it and reading it unguarded would blank the panel.
  const sharedServices = graph?.sharedServices ?? []

  // Providers per service, and which (plugin, service) pairs a plugin provides
  // for itself. Real DSH packages do require a service they also provide
  // (`dsh-session` injects `sessions`), and the daemon treats that as already
  // satisfied; drawing both directions anyway would fabricate a two-node cycle
  // and mark it as one.
  const selfProvided = useMemo(() => {
    const keys = new Set<string>()
    for (const edge of graph?.provides ?? []) keys.add(`${edge.plugin}\u0000${edge.service}`)
    return keys
  }, [graph])

  const shownPlugins = useMemo(
    () => new Set(visible.plugins.map((plugin) => displayId(plugin.id))),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [visible.plugins, halfOf],
  )

  // Both views read left to right as load order: prerequisites first, dependents
  // after. The plugin view therefore draws its edge from the provider to the
  // plugin that requires it, matching the direction the service view flows
  // through the hub — and matching the load-order list below the diagram.
  const edges: LayoutEdge[] = useMemo(() => {
    if (graph === null) return []
    if (view === 'plugins') {
      // A repeated pair collapses inside the layout, which combines both the
      // service labels and the hover text.
      return graph.links
        .filter((link) => shownPlugins.has(displayId(link.from)) && shownPlugins.has(displayId(link.to)))
        .filter((link) => displayId(link.from) !== displayId(link.to))
        .map((link) => ({
          from: displayId(link.to),
          to: displayId(link.from),
          label: link.service,
          title: text.pluginGraphEdge(
            fullName(link.from),
            fullName(link.to),
            link.service,
          ),
        }))
    }
    return [
      ...graph.provides
        .filter((edge) => shownPlugins.has(displayId(edge.plugin)))
        .map((edge) => ({
          from: displayId(edge.plugin),
          to: edge.service,
          title: text.pluginGraphProvidesEdge(fullName(edge.plugin), edge.service),
        })),
      ...graph.requires
        .filter((edge) => shownPlugins.has(displayId(edge.plugin)))
        .filter((edge) => !selfProvided.has(`${edge.plugin}\u0000${edge.service}`))
        .map((edge) => ({
          from: edge.service,
          to: displayId(edge.plugin),
          title: text.pluginGraphRequiresEdge(fullName(edge.plugin), edge.service),
        })),
    ]
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, view, text, selfProvided, shownPlugins, halfOf])

  // The service view adds a pill per service. Services nothing left in the graph
  // touches would be dead ends, so they follow the same filter as the plugins.
  // Derived from the edge list rather than re-filtering the graph, so the two
  // views can never disagree about what is drawn.
  const serviceNodes = useMemo(() => {
    if (graph === null) return []
    const touched = new Set<string>()
    for (const edge of edges) {
      if (graph.services.includes(edge.from)) touched.add(edge.from)
      if (graph.services.includes(edge.to)) touched.add(edge.to)
    }
    return graph.services.filter((service) => touched.has(service))
  }, [graph, edges])

  const nodes = useMemo(() => {
    if (graph === null) return []
    const names = visible.plugins.map((plugin) => displayId(plugin.id))
    const unique = [...new Set(names)]
    return view === 'services' ? [...unique, ...serviceNodes] : unique
  }, [graph, view, visible.plugins, serviceNodes])

  // How many plugins depend on each node, transitively — the layout's
  // within-layer key. A graph this wide is mostly one layer deep, and without a
  // gradient inside it the base reads as a crowd; `dsh-invariants` is depended on
  // by 77 plugins and `dsh-bash-local` by one, and the picture should say so.
  const weight = useMemo(() => {
    const dependents = new Map<string, Set<string>>()
    for (const link of graph?.links ?? []) {
      const consumer = displayId(link.from)
      const provider = displayId(link.to)
      if (consumer === provider) continue
      const set = dependents.get(provider) ?? new Set<string>()
      set.add(consumer)
      dependents.set(provider, set)
    }
    const total = new Map<string, number>()
    for (const id of nodes) {
      const seen = new Set<string>([id])
      const stack = [id]
      while (stack.length > 0) {
        for (const consumer of dependents.get(stack.pop()!) ?? []) {
          if (seen.has(consumer)) continue
          seen.add(consumer)
          stack.push(consumer)
        }
      }
      total.set(id, seen.size - 1)
    }
    return total
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, halfOf, nodes])

  const layout = useMemo(
    () => layoutGraph({ nodes, edges, order: graph?.order ?? [], weight }),
    [nodes, edges, graph, weight],
  )

  // Is the node actually drawn right now? Distinct from "is it in the graph":
  // a filter or the view decides whether it has a box, and only a box can be
  // centred or outlined.
  const drawn = useMemo(() => new Set(layout.nodes.map((node) => node.id)), [layout])

  // Centre a node in the stage. A search hit or a diagnostic entry has to land
  // somewhere the reader can see, not at whatever pan they happened to leave.
  const focusNode = (target: string): void => {
    const node_ = displayId(target)
    setSelected(node_)
    const container = canvasRef.current
    const node = layout.nodes.find((entry) => entry.id === node_)
    if (container === null || node === undefined) return
    setTransform((current) => clampTransform({
      scale: current.scale,
      x: container.clientWidth / 2 - (node.x + NODE_WIDTH / 2) * current.scale,
      y: container.clientHeight / 2 - (node.y + NODE_HEIGHT / 2) * current.scale,
    }))
  }

  // Go to a node named somewhere other than the diagram — a diagnostic row, a
  // conflict, the load order. Revealing a filtered-out node and centring it take
  // two passes, so that case parks the request in `pendingFocus`.
  const requestFocus = (target: string, nextView?: GraphView): void => {
    if (nextView !== undefined && nextView !== view) {
      pendingFocus.current = target
      setView(nextView)
      return
    }
    if (drawn.has(displayId(target))) {
      focusNode(target)
      return
    }
    pendingFocus.current = target
    if (!shownPlugins.has(target)) setShowInactive(true)
    if (!participants.has(target)) setShowIsolated(true)
  }

  // Consume a parked focus once the layout that contains the node exists. Deferred
  // like `fit` so it lands after the fit both effects are triggered by, which
  // would otherwise move the view straight back.
  useEffect(() => {
    const target = pendingFocus.current
    if (target === null || !drawn.has(target)) return
    pendingFocus.current = null
    const handle = window.setTimeout(() => { focusNode(target) }, 0)
    return () => window.clearTimeout(handle)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [drawn])

  // What the search box matches. Matching narrows nothing: hiding the rest would
  // relayout the diagram on every keystroke and take away the context the hit is
  // read in, so a hit is outlined and centred instead.
  const search = useMemo(() => {
    const needle = query.trim().toLowerCase()
    if (needle === '') return null
    const matched = new Set<string>()
    for (const plugin of visible.plugins) {
      // Through `displayId`, and so deduplicated: merged, both halves of a
      // dual-face package are one box, and a set of raw ids would outline only
      // whichever half happens to be keyed by a drawn node.
      if (plugin.name.toLowerCase().includes(needle)) matched.add(displayId(plugin.id))
    }
    if (view === 'services') {
      for (const service of serviceNodes) {
        if (service.toLowerCase().includes(needle)) matched.add(service)
      }
    }
    const hidden = (graph?.plugins ?? [])
      .filter((plugin) => !drawn.has(displayId(plugin.id)) && plugin.name.toLowerCase().includes(needle))
      .map((plugin) => plugin.name)
    return { matched, hidden }
  }, [query, visible.plugins, serviceNodes, graph, drawn, view])

  // Typing into the box should take the reader to the hit rather than leave them
  // to find the outline in a 144-node diagram.
  useEffect(() => {
    const needle = query.trim().toLowerCase()
    if (needle === '') return
    const hit = visible.plugins.find((plugin) => plugin.name.toLowerCase().includes(needle))
    if (hit !== undefined) focusNode(hit.id)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query])

  const meta = (nodeId: string): GraphNodeMeta => {
    if (graph?.services.includes(nodeId) ?? false) {
      return {
        label: nodeId,
        kind: 'service',
        issue: missingServices.has(nodeId) ? 'missing' : inactiveServices.has(nodeId) ? 'inactive-provider' : null,
      }
    }
    const plugin = byId.get(nodeId)
    // Only an activated plugin actually loads, so only its unmet requirement is a
    // real hang. Marking every consumer would paint the whole diagram red over
    // packages that are merely absent from this tree. A cycle member is marked
    // whatever its activation: the cycle is why no load order exists at all.
    const issueOf = (id: string): GraphNodeMeta['issue'] => {
      if (cycleMembers.has(id)) return 'cycle'
      if (!(byId.get(id)?.activated ?? false)) return null
      if (marks.missing.has(id)) return 'missing'
      if (marks.inactiveProvider.has(id)) return 'inactive-provider'
      return null
    }
    // A merged box stands for both halves, so it carries whichever half is marked.
    // The halves never share a cycle — that is what resolving within a context
    // means — but one of them can be blocked while the other is fine.
    const ranked: GraphNodeMeta['issue'][] = ['cycle', 'missing', 'inactive-provider']
    const issues = halvesOf(nodeId).map((half) => issueOf(half.id))
    const issue = ranked.find((kind) => issues.includes(kind)) ?? null
    const both = halvesOf(nodeId).length > 1
    return {
      label: nodeLabel(nodeId),
      title: both ? `${fullName(nodeId)} (${text.pluginGraphHalvesBoth})` : fullName(nodeId),
      kind: 'plugin',
      activated: plugin?.activated ?? false,
      issue,
    }
  }

  // Fit the diagram to the stage, so a wide graph is readable without hunting for
  // the zoom level first.
  const fit = (): void => {
    const container = canvasRef.current
    if (container === null || layout.width === 0) return
    const availableWidth = container.clientWidth - 8
    const availableHeight = container.clientHeight - 8
    const scale = Math.max(
      MIN_READABLE_SCALE,
      Math.min(
        1,
        availableWidth / layout.width,
        layout.height > 0 ? availableHeight / layout.height : 1,
      ),
    )
    // Centre whatever fits; anything past the edge stays reachable by panning.
    setTransform({
      scale,
      x: Math.max(0, (availableWidth - layout.width * scale) / 2),
      y: Math.max(0, (availableHeight - layout.height * scale) / 2),
    })
  }

  useEffect(() => {
    if (graph === null) return
    setSelected(null)
    // Defer so the container has been laid out and clientWidth is real.
    const handle = window.setTimeout(fit, 0)
    return () => window.clearTimeout(handle)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, view, showIsolated, showInactive])

  // Wheel handling is attached natively because React's synthetic wheel listener
  // is passive, and a passive listener cannot cancel the dialog's own scrolling.
  //
  // The effect is keyed on the graph, not on nothing: the stage only exists once
  // a graph has loaded, so a mount-time effect found `canvasRef.current === null`
  // and never attached anything — the wheel was inert in both directions.
  //
  // Wheel pans and key-held wheel zooms, which is what a trackpad (two fingers =
  // wheel, pinch = ctrl+wheel) and a mouse already mean to do. Plain wheel has to
  // pan: the diagram is larger than the stage, so a wheel that only zoomed left
  // no way to reach the off-screen part except dragging.
  useEffect(() => {
    const container = canvasRef.current
    if (container === null) return
    const onWheel = (event: WheelEvent) => {
      // The detail panel overlays the stage and scrolls on its own. This listener
      // is attached natively, so it runs on the way up to the container before any
      // React handler could stop it — the panel has to be excluded here.
      if (event.target instanceof Element && event.target.closest('.plugin-graph-detail') !== null) return
      event.preventDefault()
      if (event.ctrlKey || event.metaKey) {
        // Zoom about the pointer, so the node under the cursor stays put.
        const rect = container.getBoundingClientRect()
        const pointerX = event.clientX - rect.left
        const pointerY = event.clientY - rect.top
        setTransform((current) => {
          const scale = clampScale(current.scale * (event.deltaY < 0 ? 1.1 : 1 / 1.1))
          const ratio = scale / current.scale
          return clampTransform({
            scale,
            x: pointerX - (pointerX - current.x) * ratio,
            y: pointerY - (pointerY - current.y) * ratio,
          })
        })
        return
      }
      setTransform((current) => clampTransform({
        ...current,
        x: current.x - event.deltaX,
        y: current.y - event.deltaY,
      }))
    }
    container.addEventListener('wheel', onWheel, { passive: false })
    return () => container.removeEventListener('wheel', onWheel)
  }, [graph])

  // Pointer state for panning. `moved` separates a pan from a click: the node
  // under the pointer receives the click either way, and selecting it would dim
  // the whole diagram the moment someone starts dragging from a node.
  // Keep the diagram reachable. Panning is unconstrained otherwise, and the
  // graph can be pushed entirely out of the stage with nothing left to grab.
  // Content smaller than the stage is centred instead, so no blank gap opens.
  const clampTransform = (next: { scale: number; x: number; y: number }): { scale: number; x: number; y: number } => {
    const container = canvasRef.current
    const content = container?.firstElementChild as HTMLElement | null | undefined
    if (!container || !content) return next
    const width = content.offsetWidth * next.scale
    const height = content.offsetHeight * next.scale
    return {
      scale: next.scale,
      x: width <= container.clientWidth
        ? (container.clientWidth - width) / 2
        : Math.min(0, Math.max(container.clientWidth - width, next.x)),
      y: height <= container.clientHeight
        ? (container.clientHeight - height) / 2
        : Math.min(0, Math.max(container.clientHeight - height, next.y)),
    }
  }

  const drag = useRef<{ x: number; y: number } | null>(null)
  const dragged = useRef(false)

  const selectUnlessDragged = (id: string | null): void => {
    if (dragged.current) return
    setSelected(id)
  }

  const selectedPlugin = selected !== null ? byId.get(selected) : undefined
  const selectedHalves = selected !== null ? halvesOf(selected) : []
  const selectedIsService = selected !== null && (graph?.services.includes(selected) ?? false)

  // The path a selected cycle member sits on, walked through the links inside its
  // group and closed back at the member. A group can be reported without a simple
  // path through it, in which case the panel says nothing rather than guessing.
  const cyclePath = useMemo(() => {
    if (selected === null || graph === null) return null
    const group = graph.cycles.find((cycle) => cycle.includes(selected))
    if (group === undefined) return null
    const members = new Set(group)
    const path = [selected]
    let current = selected
    for (let step = 0; step < group.length; step += 1) {
      const candidates = graph.links.filter(
        (link) => link.from === current && members.has(link.to) && link.to !== current,
      )
      // Prefer the edge that closes the loop: a member can lead into the cycle
      // from outside it, and following that branch first walks away from the
      // selected node instead of around it.
      const next = candidates.find((link) => link.to === selected) ?? candidates[0]
      if (next === undefined) return null
      current = next.to
      path.push(current)
      if (current === selected) return path
    }
    return null
  }, [graph, selected])
  const hasIssues =
    (graph?.missing.length ?? 0) > 0 ||
    (graph?.inactiveProviders.length ?? 0) > 0 ||
    (graph?.cycles.length ?? 0) > 0

  // One entry per drawn node: merged, a package's two halves would otherwise
  // appear twice in a list that is read as "the order".
  const orderEntries = useMemo(() => {
    const seen = new Set<string>()
    const entries: string[] = []
    for (const id of graph?.order ?? []) {
      const shown = displayId(id)
      if (seen.has(shown)) continue
      seen.add(shown)
      entries.push(shown)
    }
    return entries
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, halfOf])

  // Keep the load order list pointing at whatever the reader selected in the
  // diagram, so "where does this sit in the order" is answered by looking rather
  // than by scrolling 144 rows.
  const selectedOrderRef = useRef<HTMLLIElement | null>(null)
  useEffect(() => {
    if (selected === null) return
    selectedOrderRef.current?.scrollIntoView({ block: 'nearest' })
  }, [selected, graph])

  return (
    <Dialog open title={text.pluginGraphTitle(id)} onClose={onClose} size="lg">
      <div className="plugin-graph-toolbar">
        <div className="plugin-graph-views">
          <Button variant="ghost" size="sm" className={view === 'plugins' ? 'active' : ''} onClick={() => { setView('plugins') }}>{text.pluginGraphViewPlugins}</Button>
          <Button variant="ghost" size="sm" className={view === 'services' ? 'active' : ''} onClick={() => { setView('services') }}>{text.pluginGraphViewServices}</Button>
        </div>
        <div className="plugin-graph-actions">
          <Input
            size="sm"
            value={query}
            placeholder={text.pluginGraphSearch}
            aria-label={text.pluginGraphSearch}
            onChange={(event) => { setQuery(event.target.value) }}
            onKeyDown={(event) => { if (event.key === 'Escape') setQuery('') }}
          />
          <Button variant="ghost" size="sm" className={showInactive ? 'active' : ''} onClick={() => { setShowInactive((current) => !current) }}>{text.pluginGraphShowInactive}</Button>
          <Button variant="ghost" size="sm" className={showIsolated ? 'active' : ''} onClick={() => { setShowIsolated((current) => !current) }}>{text.pluginGraphShowIsolated}</Button>
          <Button
            variant="ghost"
            size="sm"
            className={splitHalves ? 'active' : ''}
            aria-pressed={splitHalves}
            onClick={() => { setSplitHalves((current) => !current) }}
          >
            {text.pluginGraphSplitHalves}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className={showHelp ? 'active' : ''}
            aria-expanded={showHelp}
            onClick={() => { setShowHelp((current) => !current) }}
          >
            {text.pluginGraphHelp}
          </Button>
          <Button variant="secondary" size="sm" onClick={fit}>{text.pluginGraphFit}</Button>
        </div>
      </div>

      <ul className="plugin-graph-legend">
        <li><span className="swatch plugin" />{text.pluginGraphLegendPlugin}</li>
        <li><span className="swatch service" />{text.pluginGraphLegendService}</li>
        <li><span className="swatch inactive" />{text.pluginGraphLegendInactive}</li>
        {marks.missingCount > 0 && (
          <li><span className="swatch mark-missing" />{text.pluginGraphLegendMissing(marks.missingCount)}</li>
        )}
        {marks.inactiveProviderCount > 0 && (
          <li><span className="swatch mark-inactive-provider" />{text.pluginGraphLegendInactiveProvider(marks.inactiveProviderCount)}</li>
        )}
        {marks.cycleCount > 0 && (
          <li><span className="swatch mark-cycle" />{text.pluginGraphLegendCycle(marks.cycleCount)}</li>
        )}
      </ul>

      {/* The how-to-read prose used to sit above the canvas permanently: 84px of
          it, out of a 441px stage, before the first node was visible. It is worth
          reading once, so it is available rather than always on screen. */}
      {showHelp && (
        <div className="plugin-graph-help">
          <p className="plugin-graph-direction">{text.pluginGraphDirectionHint}</p>
          <p className="plugin-graph-direction">{text.pluginGraphOrderHint}</p>
          {/* Each red mark is spelled out where it is drawn: one swatch labelled
              "blocked" covered three different conditions and explained none. */}
          <ul className="plugin-graph-legend-notes">
            {marks.missingCount > 0 && <li>{text.pluginGraphMissingHint}</li>}
            {marks.inactiveProviderCount > 0 && <li>{text.pluginGraphInactiveProviderHint}</li>}
            {marks.cycleCount > 0 && (
              <>
                <li>{text.pluginGraphCycleHint}</li>
                <li>{text.pluginGraphBackEdgeHint}</li>
              </>
            )}
          </ul>
        </div>
      )}

      {loading && <p className="plugin-graph-note">{text.pluginGraphLoading}</p>}
      {error !== null && <p className="plugin-graph-error">{text.pluginGraphError(error)}</p>}

      {graph !== null && error === null && (
        <>
          {(visible.hiddenInactive > 0 || visible.hiddenIsolated > 0) && (
            // One line rather than two: both are the same kind of notice — what
            // this view is leaving out — and the reader needs the fact, not a
            // paragraph per reason.
            <p className="plugin-graph-note">
              {[
                visible.hiddenInactive > 0 ? text.pluginGraphHiddenInactive(visible.hiddenInactive) : null,
                visible.hiddenIsolated > 0 ? text.pluginGraphHiddenIsolated(visible.hiddenIsolated) : null,
              ].filter((line): line is string => line !== null).join(' ')}
            </p>
          )}
          {search !== null && (
            // A search that finds only hidden plugins looks like a search that
            // found nothing, so the omission is named rather than left to guess.
            <p className="plugin-graph-note">
              {text.pluginGraphMatch(search.matched.size)}
              {search.hidden.length > 0 ? ` · ${text.pluginGraphSearchHidden(search.hidden.length)}` : ''}
            </p>
          )}

          <div className="plugin-graph-body">
            <div className="plugin-graph-main">
            <div
              ref={canvasRef}
              className="plugin-graph-stage"
              onPointerDown={(event) => {
                drag.current = { x: event.clientX, y: event.clientY }
                dragged.current = false
              }}
              onPointerMove={(event) => {
                const start = drag.current
                if (start === null) return
                const dx = event.clientX - start.x
                const dy = event.clientY - start.y
                if (Math.abs(dx) > DRAG_THRESHOLD_PX || Math.abs(dy) > DRAG_THRESHOLD_PX) {
                  dragged.current = true
                }
                setTransform((current) => clampTransform({ ...current, x: current.x + dx, y: current.y + dy }))
                drag.current = { x: event.clientX, y: event.clientY }
              }}
              onPointerUp={() => { drag.current = null }}
              onPointerLeave={() => { drag.current = null }}
            >
              <div className="plugin-graph-zoom" style={{ transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})` }}>
                <PluginGraphView
                  layout={layout}
                  meta={meta}
                  selected={selected}
                  onSelect={selectUnlessDragged}
                  showEdgeLabels={layout.edges.length <= EDGE_LABEL_LIMIT}
                  layerLabel={text.pluginGraphLayerLabel}
                  matches={search?.matched ?? null}
                  canvasLabel={text.pluginGraphCanvas}
                  emptyLabel={text.pluginGraphEmpty}
                />
              </div>

              {/* Inside the stage rather than below it: as a sibling it took its
                  height out of the stage, so opening the detail shrank the canvas
                  from 441px to 300px while the zoom stayed where it was — the
                  bottom of the diagram went off the edge and the view was no
                  longer centred. Overlaying keeps the diagram's own geometry
                  fixed while the reader clicks around it. */}
              {selected !== null && (
                <div
                  className="plugin-graph-detail"
                  onPointerDown={(event) => { event.stopPropagation() }}
                >
                  <div className="plugin-graph-detail-head">
                    <strong>{fullName(selected)}</strong>
                    {selectedIsService && <Badge variant="neutral">{text.pluginGraphLegendService}</Badge>}
                    {selectedPlugin !== undefined && (
                      <Badge variant={selectedPlugin.activated ? 'success' : 'neutral'}>
                        {selectedPlugin.activated ? text.pluginGraphActivated : text.pluginGraphInactiveNode}
                      </Badge>
                    )}
                  </div>
                  {selectedIsService ? (
                    <>
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphProvides}</span>{' '}
                        {(serviceProviders.get(selected) ?? []).join(', ') || text.pluginGraphNothing}
                      </p>
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphConsumedBy}</span>{' '}
                        {(serviceConsumers.get(selected) ?? []).join(', ') || text.pluginGraphNothing}
                      </p>
                    </>
                  ) : selectedHalves.length > 1 ? (
                    // The merged box's honest breakdown: which half registers what.
                    // The two are different cordis plugins, so one list would claim
                    // the package does something neither half does.
                    <>
                      {selectedHalves.map((half) => (
                        <p className="plugin-graph-detail-line" key={half.id}>
                          <span className="label">
                            {half.half === 'client' ? text.pluginGraphHalfClient : text.pluginGraphHalfHost}
                          </span>{' '}
                          {(half.provides.join(', ') || text.pluginGraphNothing)}
                          {' ← '}
                          {(half.requires.join(', ') || text.pluginGraphNothing)}
                        </p>
                      ))}
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphSource}</span> <code>{selectedHalves[0].source}</code>
                      </p>
                      {cyclePath !== null && (
                        <p className="plugin-graph-detail-line danger">
                          <span className="label">{text.pluginGraphDetailCycle}</span>{' '}
                          <code>{cyclePath.map((id) => fullName(id)).join(' → ')}</code>
                        </p>
                      )}
                    </>
                  ) : selectedPlugin !== undefined ? (
                    <>
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphProvides}</span>{' '}
                        {selectedPlugin.provides.join(', ') || text.pluginGraphNothing}
                      </p>
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphRequires}</span>{' '}
                        {selectedPlugin.requires.join(', ') || text.pluginGraphNothing}
                      </p>
                      <p className="plugin-graph-detail-line">
                        <span className="label">{text.pluginGraphSource}</span> <code>{selectedPlugin.source}</code>
                      </p>
                      {cyclePath !== null && (
                        // Why this node is red, spelled out: the path it sits on.
                        <p className="plugin-graph-detail-line danger">
                          <span className="label">{text.pluginGraphDetailCycle}</span>{' '}
                          <code>{cyclePath.map((id) => fullName(id)).join(' → ')}</code>
                        </p>
                      )}
                    </>
                  ) : null}
                </div>
              )}
            </div>

            </div>
            <aside className="plugin-graph-side">
            <div className="plugin-graph-diagnostics">
              <h3>{text.pluginGraphDiagnostics}</h3>
              {!hasIssues && <p className="plugin-graph-ok">{text.pluginGraphNoIssues}</p>}
              {graph.missing.length > 0 && (
                <div className="plugin-graph-issue">
                  <Badge variant="danger">{text.pluginGraphMissing(graph.missing.length)}</Badge>
                  <ul>
                    {graph.missing.map((edge) => (
                      <li key={`${edge.plugin}-${edge.service}`}>
                        <button type="button" onClick={() => { requestFocus(edge.plugin) }}>
                          {text.pluginGraphBlocked(fullName(edge.plugin), edge.service)}
                        </button>
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {graph.inactiveProviders.length > 0 && (
                <div className="plugin-graph-issue">
                  <Badge variant="danger">{text.pluginGraphInactive(graph.inactiveProviders.length)}</Badge>
                  <ul>
                  {graph.inactiveProviders.map((edge) => (
                    <li key={`${edge.plugin}-${edge.service}`}>
                      <button type="button" onClick={() => { requestFocus(edge.plugin) }}>
                        {text.pluginGraphBlockedInactive(fullName(edge.plugin), edge.service)}
                      </button>
                    </li>
                  ))}
                  </ul>
                </div>
              )}
              {graph.cycles.length > 0 && (
                <div className="plugin-graph-issue">
                  <Badge variant="danger">{text.pluginGraphCycles(graph.cycles.length)}</Badge>
                  <ul>
                    {graph.cycles.map((cycle) => (
                      <li key={cycle.join('+')}>
                        {cycle.map((name, index) => (
                          <Fragment key={name}>
                            {index > 0 && ' → '}
                            <button type="button" onClick={() => { requestFocus(name) }}>{fullName(name)}</button>
                          </Fragment>
                        ))}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {/* Not an error, and not a conflict: cordis scopes a service name to
                  one context, so a host implementation and a browser one coexist
                  by design. It is worth saying because this diagram merges those
                  contexts, and that is where the extra lines come from — and often
                  where a cycle comes from too. */}
              {sharedServices.length > 0 && (
                <div className="plugin-graph-issue">
                  <Badge variant="neutral">{text.pluginGraphSharedServices(sharedServices.length)}</Badge>
                  <p className="plugin-graph-note">{text.pluginGraphSharedServiceHint}</p>
                  <ul>
                    {sharedServices.map((service) => (
                      <li key={service.service}>
                        {/* The service name is the way in: its owners are separate
                            plugins and either one is arbitrary, so the jump goes
                            to the hub that both of them register. */}
                        <button type="button" onClick={() => { requestFocus(service.service, 'services') }}>
                          {service.service}
                        </button>
                        {' '}
                        {text.pluginGraphSharedService(service.providers.map((id) => fullName(id)).join(', '))}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {graph.diagnostics.length > 0 && (
                <div className="plugin-graph-issue">
                  {/* 27 rows of them on the real container, and every one is a
                      limit of reading source statically rather than something the
                      reader can act on. Collapsed, they stop being the largest
                      block in the column while staying a click away. */}
                  <button
                    type="button"
                    className="plugin-graph-disclosure"
                    aria-expanded={showParseNotes}
                    onClick={() => { setShowParseNotes((current) => !current) }}
                  >
                    <Badge variant="neutral">{text.pluginGraphParseNotes(graph.diagnostics.length)}</Badge>
                  </button>
                  {showParseNotes && (
                    <>
                      <p className="plugin-graph-note">{text.pluginGraphParseNotesHint}</p>
                      <ul>
                        {graph.diagnostics.map((line) => <li key={line}>{line}</li>)}
                      </ul>
                    </>
                  )}
                </div>
              )}
            </div>

            <div className="plugin-graph-order">
              <h3>{text.pluginGraphOrder}</h3>
              {graph.order.length > 0 && (
                <ol>
                  {orderEntries.map((name) => (
                    // The order covers every plugin in the graph, so entries that
                    // never load are muted rather than dropped: the chain through
                    // them is still what the declarations imply. Cycle members are
                    // the entries with no order of their own, so they say so
                    // instead of passing off their position as meaningful.
                    <li
                      key={name}
                      ref={name === selected ? selectedOrderRef : undefined}
                      className={[
                        byId.get(name)?.activated === false ? 'inactive' : '',
                        cycleMembers.has(name) ? 'pending' : '',
                        name === selected ? 'current' : '',
                      ].filter(Boolean).join(' ') || undefined}
                      title={cycleMembers.has(name) ? text.pluginGraphOrderPending : undefined}
                    >
                      <code>{fullName(name)}</code>
                    </li>
                  ))}
                </ol>
              )}
              {graph.order.length === 0 && (
                <p className="plugin-graph-note danger">{text.pluginGraphOrderUnavailable}</p>
              )}
            </div>
            </aside>
          </div>
        </>
      )}

      <div className="plugin-graph-footer">
        <Button variant="secondary" size="sm" onClick={onClose}>{text.close}</Button>
      </div>
    </Dialog>
  )
}
