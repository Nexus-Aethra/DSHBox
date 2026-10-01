/**
 * Agent-facing tools that bridge to dshbox's page-debugging RPCs.
 *
 * These are plain ctx.tools.register contributions. They deliberately do not
 * claim ctx.computerUse or ctx.browserUse: both are exclusive single-slot
 * registries, and the browser we drive here is a throwaway headless process
 * used to inspect one container's page - it controls no host screen, mouse
 * or keyboard. Taking such a slot would claim a capability we do not have
 * and would make a genuine computer-use provider fail to load with
 * 'already registered'.
 *
 * Every tool acts on a container by id and needs a headless session.
 * Sessions open on first use rather than being exposed as a lifecycle the
 * model has to drive: a debugging tool that fails because the agent forgot
 * a setup step is a tool that gets used wrong.
 */

import type { Context } from '@deepseek-ai/cordis'

import { getRpc } from './rpc'

/**
 * The host contract this plugin registers against, described structurally.
 *
 * DSH expresses these services as cordis module augmentations declared by
 * @deepseek-ai/dsh-tools and @deepseek-ai/dsh-attachment. Those packages are
 * harness workspace members using the workspace: protocol and are published to
 * no registry; a plugin installed into a container is installed into that
 * container's own pnpm project, which cannot see the harness workspace, so
 * naming them as dependencies fails to install with a 404. Depending on the
 * type packages just to spell these two service shapes is not worth making the
 * plugin uninstallable.
 *
 * The shapes below are transcribed from the host declarations. They cover only
 * what this plugin uses; if the host changes them, the cast in hostServices()
 * is the single place that notices, and the tools fail loudly at registration
 * rather than silently going missing.
 */
interface ImageAttachmentRef {
  attachmentId: string
  mediaType: 'image/png' | 'image/jpeg' | 'image/webp' | 'image/gif'
  bytes: number
  width: number
  height: number
  name?: string
}

interface HostToolDefinition {
  name: string
  description: string
  /** JSON Schema object for the arguments, as sent to the model. */
  parameters: Record<string, unknown>
  /** Cooperative budget in ms; omit for no deadline. */
  timeoutMs?: number
  output: {
    schema: unknown
    render(args: unknown, value: Record<string, unknown>): unknown[]
  }
  execute(args: unknown, exec: unknown): Promise<unknown>
}

interface HostServices {
  tools: { register(definition: HostToolDefinition): () => void }
  attachments: {
    saveImage(input: {
      data: Uint8Array
      mediaType: ImageAttachmentRef['mediaType']
      name?: string
    }): Promise<ImageAttachmentRef>
  }
}

/**
 * Reach the two host services this plugin needs.
 *
 * This is the only place the structural contract touches cordis. Keeping the
 * assertion here means the rest of the file is checked against the transcribed
 * types, so a shape change surfaces as one cast rather than a scatter of them.
 */
function hostServices(ctx: Context): HostServices {
  return ctx as unknown as HostServices
}

/** Long enough for a cold browser launch on the first call of a session. */
const LAUNCH_BUDGET_MS = 90_000
/** A click or a query against an already-warm session is fast. */
const WARM_BUDGET_MS = 30_000

interface ElementSummary {
  tag?: string
  id?: string | null
  classes?: string | null
  text?: string
  visible?: boolean
  width?: number
  height?: number
  centerX?: number
  centerY?: number
}

function containerIdOf(args: unknown): string {
  const id = (args as { containerId?: unknown } | undefined)?.containerId
  if (typeof id !== 'string' || id.trim() === '')
    throw new Error('containerId is required')
  return id
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : {}
}

function textOf(value: unknown): string | undefined {
  const raw = asRecord(value).text
  return typeof raw === 'string' ? raw : undefined
}


/**
 * Call a debug RPC, opening the container's headless session if needed.
 *
 * The daemon keeps one session per container, so a missing session is the
 * normal first-call state rather than an error worth surfacing. Re-opening
 * is idempotent, which also recovers a session whose browser died.
 */
async function withSession<T>(
  id: string,
  method: string,
  params: Record<string, unknown>,
): Promise<T> {
  const rpc = getRpc()
  try {
    return await rpc.call<T>(method, { id, ...params })
  } catch (error) {
    if (!/no debug session/i.test(String(error))) throw error
    await rpc.call('debug_open', { id })
    return await rpc.call<T>(method, { id, ...params })
  }
}

function renderElements(elements: ElementSummary[]): string {
  if (elements.length === 0) return 'No elements matched.'
  return elements
    .map((element, index) => {
      const tag = element.tag ?? '?'
      const id = element.id ? '#' + element.id : ''
      const cls = element.classes ? '.' + element.classes.split(/\s+/).join('.') : ''
      const where = element.visible === false ? ' (hidden)' : ''
      const size = element.width !== undefined && element.height !== undefined
        ? ' ' + Math.round(element.width) + 'x' + Math.round(element.height)
        : ''
      const text = element.text ? ' ' + JSON.stringify(element.text) : ''
      const at = Math.round(element.centerX ?? 0) + ',' + Math.round(element.centerY ?? 0)
      return '[' + index + '] <' + tag + id + cls + '>' + size + ' at (' + at + ')' + where + text
    })
    .join('\n')
}
interface ElementQuery {
  selector: string
  count: number
  truncated: boolean
  elements: ElementSummary[]
}

interface ClickResult {
  selector: string
  x: number
  y: number
  tag: string
  text: string
}


function base64Of(value: unknown): string {
  const data = asRecord(value).data
  if (typeof data !== 'string') throw new Error('screenshot result carried no image data')
  return data
}

/**
 * Register one tool against the host registry.
 *
 * Kept as a named function so each definition is typed as ToolDefinition at the
 * call site, which is what makes a missing field a compile error here rather
 * than a silent failure the first time the model calls the tool.
 */
function registerTool(ctx: Context, definition: HostToolDefinition): void {
  hostServices(ctx).tools.register(definition)
}

export function applyBoxTools(ctx: Context): void {
  registerTool(ctx, {
    name: 'box_screenshot',
    description:
      'Capture what a running dshbox container currently renders, as a PNG you can look at. '
      + 'Use it to see the real state of a page instead of guessing from logs or markup. '
      + 'Opens a headless browser on the container on first use.',
    parameters: {
      containerId: {
        type: 'string',
        required: true,
        description: 'Container id to look at, e.g. from a container list.',
      },
      fullPage: {
        type: 'boolean',
        description: 'Capture the whole scrollable page instead of the viewport. Defaults to false.',
      },
    },
    timeoutMs: LAUNCH_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string', required: true },
          attachment: { type: 'object', required: true },
        },
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const shot = asRecord(record.attachment)
        return [
          {
            type: 'text',
            text: 'Screenshot of container ' + String(record.containerId)
              + ' at ' + String(shot.width) + 'x' + String(shot.height) + '.',
          },
          { type: 'image', attachment: record.attachment as ImageAttachmentRef },
        ]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      const raw = await withSession(containerId, 'debug_capture_screenshot', {
        format: 'png',
        fullPage: record.fullPage === true,
      })
      // Image bytes cannot ride back on a tool result: the attachment store
      // owns validation and normalization, and every model route sends a
      // placeholder naming the image rather than its bytes.
      const data = Buffer.from(base64Of(raw), 'base64')
      const attachment = await hostServices(ctx).attachments.saveImage({
        data: new Uint8Array(data),
        mediaType: 'image/png',
      })
      return { containerId, attachment }
    },
  })
  registerTool(ctx, {
    name: 'box_query_elements',
    description:
      'List the elements a CSS selector matches on a running dshbox container page, with each '
      + 'tag, classes, visible text and centre coordinates. Use it to find the selector to '
      + 'click, or to confirm a selector still matches after a change. A broad selector such '
      + 'as body returns a capped sample rather than the whole tree.',
    parameters: {
      containerId: { type: 'string', required: true, description: 'Container id to inspect.' },
      selector: {
        type: 'string',
        description: 'CSS selector to match, e.g. button or [role=button]. Defaults to body.',
      },
      limit: {
        type: 'number',
        description: 'Maximum elements to return; the daemon caps this at 200. Defaults to 50.',
      },
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string', required: true },
          selector: { type: 'string', required: true },
          count: { type: 'number', required: true },
          truncated: { type: 'boolean', required: true },
          elements: { type: 'array', required: true, items: { type: 'object' } },
        },
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const elements = Array.isArray(record.elements)
          ? (record.elements as ElementSummary[])
          : []
        const header = 'Matched ' + String(record.count) + ' element(s) for '
          + String(record.selector)
          + (record.truncated === true ? ' (truncated).' : '.')
        return [{ type: 'text', text: header + '\n' + renderElements(elements) }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      const result = await withSession<ElementQuery>(containerId, 'debug_query_elements', {
        selector: typeof record.selector === 'string' ? record.selector : 'body',
        limit: typeof record.limit === 'number' ? record.limit : 50,
      })
      return { containerId, ...result }
    },
  })

  registerTool(ctx, {
    name: 'box_click_element',
    description:
      'Click the first element matching a CSS selector on a running dshbox container page, '
      + 'at its centre. Prefer this over box_click_at when the target has a selector: it '
      + 'survives the layout moving. Re-screenshot or re-query afterwards to see the result.',
    parameters: {
      containerId: { type: 'string', required: true, description: 'Container id to click in.' },
      selector: {
        type: 'string',
        required: true,
        description: 'CSS selector of the element to click, e.g. button.submit or [role=button].',
      },
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string', required: true },
          selector: { type: 'string', required: true },
          x: { type: 'number', required: true },
          y: { type: 'number', required: true },
          tag: { type: 'string', required: true },
          text: { type: 'string', required: true },
        },
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const label = textOf(record.text)
        return [{
          type: 'text',
          text: 'Clicked <' + String(record.tag) + '> matching ' + String(record.selector)
            + ' at (' + String(record.x) + ', ' + String(record.y) + ') in container '
            + String(record.containerId)
            + (label ? ' labelled ' + JSON.stringify(label) : '')
            + '. Screenshot again to see what changed.',
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      const selector = record.selector
      if (typeof selector !== 'string' || selector.trim() === '')
        throw new Error('selector is required')
      const result = await withSession<ClickResult>(containerId, 'debug_click_element', { selector })
      return { containerId, ...result }
    },
  })

  registerTool(ctx, {
    name: 'box_click_at',
    description:
      'Click a point, in viewport coordinates, on a running dshbox container page. Use it when '
      + 'the target has no usable selector. Coordinates come from box_query_elements or from '
      + 'box_screenshot. Prefer box_click_element when a selector exists.',
    parameters: {
      containerId: { type: 'string', required: true, description: 'Container id to click in.' },
      x: { type: 'number', required: true, description: 'Horizontal viewport coordinate, from the left edge.' },
      y: { type: 'number', required: true, description: 'Vertical viewport coordinate, from the top edge.' },
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string', required: true },
          x: { type: 'number', required: true },
          y: { type: 'number', required: true },
        },
      },
      render: (_args, value) => {
        const record = asRecord(value)
        return [{
          type: 'text',
          text: 'Clicked (' + String(record.x) + ', ' + String(record.y)
            + ') in container ' + String(record.containerId)
            + '. Screenshot again to see what changed.',
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      if (typeof record.x !== 'number' || typeof record.y !== 'number')
        throw new Error('x and y are required numbers')
      const result = await withSession<{ x: number, y: number }>(
        containerId,
        'debug_click_at',
        { x: record.x, y: record.y },
      )
      return { containerId, ...result }
    },
  })
}
