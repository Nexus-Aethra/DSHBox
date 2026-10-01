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
  /** One prompt section carrying the cross-tool workflow. */
  systemPrompt: {
    section(section: {
      name: string
      order: number
      text: string | (() => string)
    }): () => void
  }
}

/**
 * Reach the two host services this plugin needs.
 *
 * This is the only place the structural contract touches cordis. Keeping the
 * assertion here means the rest of the file is checked against the transcribed
 * types, so a shape change surfaces as one cast rather than a scatter of them.
 */
export function hostServices(ctx: Context): HostServices {
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



/**
 * Render one page element on a line an agent can act on.
 *
 * The role is what the page says the control *is*, which is what an agent
 * should dispatch on; the coordinates are what box_click_at needs; and the
 * off-screen marker is what stops an agent from concluding that a control
 * does not exist because it happens to be below the fold.
 */

interface PageElement {
  role?: string
  name?: string
  value?: string
  x?: number
  y?: number
  width?: number
  height?: number
  inViewport?: boolean
  clickable?: boolean
}

interface PageText {
  count: number
  truncated: boolean
  belowFold: number
  elements: PageElement[]
}

interface ScrollState {
  scrollTop: number
  scrollHeight: number
  viewportHeight: number
  screensBelow: number
  moved: boolean
}

function renderPageElements(elements: PageElement[]): string {
  if (elements.length === 0) return 'No elements on this page.'
  return elements
    .map((element, index) => {
      const role = element.role ?? '?'
      const name = element.name ?? ''
      const value = element.value ? ' value=' + JSON.stringify(element.value) : ''
      const at = Math.round(element.x ?? 0) + ',' + Math.round(element.y ?? 0)
      const size = element.width !== undefined && element.height !== undefined
        ? ' ' + Math.round(element.width) + 'x' + Math.round(element.height)
        : ''
      const off = element.inViewport === false ? ' [BELOW FOLD - scroll first]' : ''
      return '[' + index + '] ' + role + ' ' + JSON.stringify(name) + value + size + ' at (' + at + ')' + off
    })
    .join('\n')
}

/**
 * The one line that keeps a response self-teaching.
 *
 * An agent meeting this toolset for the first time should learn the next
 * move from the response it already has, not from a manual it will never
 * read. So every tool ends by naming the call that usually follows.
 */
function nextHint(next: string): string {
  return '\nnext: ' + next
}

interface ClickVerification {
  landed?: boolean
  hitTag?: string
  hitText?: string
  occludedBy?: string
}

/**
 * Render a click, distinguishing the two outcomes an agent must not confuse.
 *
 * A dispatched event is not a delivered one. When something covers the target
 * -- an overlay, a cookie bar, an extension injected into the page -- the click
 * reaches that instead and the page appears to do nothing, which reads as a
 * broken app rather than a blocked click. Naming the cover turns a dead end
 * into a decision: scroll, dismiss, or target the overlay itself.
 */
function renderClick(value: Record<string, unknown>, target: string): unknown[] {
  const record = asRecord(value) as Record<string, unknown> & ClickVerification
  if (record.landed === false) {
    const cover = record.occludedBy
      ? ' it was covered by <' + String(record.occludedBy) + '>'
        + (record.hitText ? ' ' + JSON.stringify(record.hitText) : '')
      : ' something else was on top of it'
    return [{
      type: 'text',
      text: 'The click on ' + target + ' in container ' + String(record.containerId)
        + ' did NOT land:' + cover + '.'
        + nextHint('scroll with box_scroll, or dismiss the covering element, then click again'),
    }]
  }
  return [{
    type: 'text',
    text: 'Clicked ' + target + ' in container ' + String(record.containerId) + '.'
      + nextHint('call box_page_text to see what the page looks like now'),
  }]
}

export function applyBoxTools(ctx: Context): void {
  registerTool(ctx, {
    name: 'box_screenshot',
    description:
      'Capture what a running dshbox container currently renders, as a PNG you can look at. '
      + 'Use it to see the real state of a page instead of guessing from logs or markup. '
      + 'Opens a headless browser on the container on first use.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to look at, e.g. from a container list.', },
        fullPage: { type: 'boolean', description: 'Capture the whole scrollable page instead of the viewport. Defaults to false.', },
      },
      required: ['containerId'],
    },
    timeoutMs: LAUNCH_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
        containerId: { type: 'string' },
        
          attachment: { type: 'object' },
      },
      required: ['containerId', 'attachment'],
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
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to inspect.' },
        selector: { type: 'string', description: 'CSS selector to match, e.g. button or [role=button]. Defaults to body.', },
        limit: { type: 'number', description: 'Maximum elements to return; the daemon caps this at 200. Defaults to 50.', },
      },
      required: ['containerId'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
        containerId: { type: 'string' },
        
          selector: { type: 'string' },
        
          count: { type: 'number' },
        
          truncated: { type: 'boolean' },
        
          elements: { type: 'array', items: { type: 'object' } },
      },
      required: ['containerId', 'selector', 'count', 'truncated', 'elements'],
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
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to click in.' },
        selector: { type: 'string', description: 'CSS selector of the element to click, e.g. button.submit or [role=button].', },
      },
      required: ['containerId', 'selector'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
        containerId: { type: 'string' },
        
          selector: { type: 'string' },
        
          x: { type: 'number' },
        
          y: { type: 'number' },
        
          tag: { type: 'string' },
        
          text: { type: 'string' },
      },
      required: ['containerId', 'selector', 'x', 'y', 'tag', 'text'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const label = textOf(record.text)
        return renderClick(value, '<' + String(record.tag) + '> matching '
          + String(record.selector) + (label ? ' labelled ' + JSON.stringify(label) : ''))
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
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to click in.' },
        x: { type: 'number', description: 'Horizontal viewport coordinate, from the left edge.' },
        y: { type: 'number', description: 'Vertical viewport coordinate, from the top edge.' },
      },
      required: ['containerId', 'x', 'y'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
        containerId: { type: 'string' },
        
          x: { type: 'number' },
        
          y: { type: 'number' },
      },
      required: ['containerId', 'x', 'y'],
      },
      render: (_args, value) => renderClick(value, 'at ('
        + String(asRecord(value).x) + ', ' + String(asRecord(value).y) + ')'),
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

/* ------------------------------------------------------------------ */
/* Page orientation, scrolling, typing and key presses.                 */
/*                                                                        */
/* Every tool below reports what it OBSERVED, not what it attempted.     */
/* These tools are read and driven by agents other than the one that     */
/* wrote them, so a response that says 'clicked' when the click landed   */
/* on a popup is worse than no tool: the agent builds the next step on   */
/* a false belief and never recovers.                                   */
/* ------------------------------------------------------------------ */

  registerTool(ctx, {
    name: 'box_page_text',
    description:
      'CALL THIS FIRST when you need to know what is on a page. It renders the whole page as '
      + 'text: every control and region with its role, name, on-screen position, and whether it '
      + 'is currently visible. You need no selector, so it answers "what is here" in one call '
      + 'where box_query_elements needs a selector you do not have yet. Then use the '
      + 'role+name it returns with box_click_element, or the coordinates with box_click_at. '
      + 'Anything marked BELOW FOLD needs box_scroll before you can act on it.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to read.' },
        role: { type: 'string', description: 'Only roles containing this text, e.g. button or textbox. Omit for everything.' },
        limit: { type: 'number', description: 'Maximum entries to return. Defaults to 200.' },
      },
      required: ['containerId'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string' },
          count: { type: 'number' },
          truncated: { type: 'boolean' },
          belowFold: { type: 'number' },
          elements: { type: 'array', items: { type: 'object' } },
        },
        required: ['containerId', 'count', 'truncated', 'belowFold', 'elements'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const elements = Array.isArray(record.elements)
          ? (record.elements as PageElement[])
          : []
        const below = Number(record.belowFold ?? 0)
        const lines: string[] = []
        lines.push(
          'Page of container ' + String(record.containerId) + ': ' + String(record.count)
          + ' entr' + (record.count === 1 ? 'y' : 'ies')
          + (record.truncated === true ? ' (truncated at the limit).' : '.'),
        )
        if (below > 0) {
          lines.push(
            below + ' entr' + (below === 1 ? 'y is' : 'ies are') + ' below the fold; call box_scroll to reach them.',
          )
        }
        lines.push(renderPageElements(elements))
        lines.push(nextHint(
          'act on an entry with box_click_element (role+name) or box_click_at (x,y)',
        ))
        return [{ type: 'text', text: lines.join('\n') }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      const result = await withSession<PageText>(containerId, 'debug_page_text', {
        role: typeof record.role === 'string' ? record.role : undefined,
        limit: typeof record.limit === 'number' ? record.limit : undefined,
      })
      return { containerId, ...result }
    },
  })

  registerTool(ctx, {
    name: 'box_scroll',
    description:
      'Report how far a page extends and optionally scroll it. Call it with no coordinates to '
      + 'learn how many screens are below the fold, which is how you find out that a control '
      + 'exists but is not reachable yet. Call it with an absolute pixel offset to move. '
      + 'Coordinates are viewport pixels, so a position from a page listing is used as-is.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to scroll.' },
        to: { type: 'number', description: 'Absolute scroll offset in pixels. Omit to only report the extent.' },
      },
      required: ['containerId'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string' },
          scrollTop: { type: 'number' },
          scrollHeight: { type: 'number' },
          viewportHeight: { type: 'number' },
          screensBelow: { type: 'number' },
          moved: { type: 'boolean' },
        },
        required: ['containerId', 'scrollTop', 'scrollHeight', 'viewportHeight', 'screensBelow', 'moved'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const below = Number(record.screensBelow ?? 0)
        return [{
          type: 'text',
          text: 'Scroll of container ' + String(record.containerId) + ': at '
            + String(record.scrollTop) + ' of ' + String(record.scrollHeight)
            + ' px, viewport ' + String(record.viewportHeight) + ' px.'
            + (record.moved === true ? ' Moved.' : '')
            + (below > 0 ? ' ' + below + ' screen(s) still below.' : ' Nothing below the fold.')
            + nextHint('call box_page_text again to see the newly reachable entries'),
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      const result = await withSession<ScrollState>(containerId, 'debug_scroll', {
        to: typeof record.to === 'number' ? record.to : undefined,
      })
      return { containerId, ...result }
    },
  })
  registerTool(ctx, {
    name: 'box_type_text',
    description:
      'Type text into whatever field currently has focus. Click the field with box_click_element '
      + 'first, then call this. It reports which element held focus, so a value that landed '
      + 'nowhere is visible as a failure rather than a silent no-op. For a submit keystroke use '
      + 'box_press_key with Enter, which is what a form expects.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to type into.' },
        text: { type: 'string', description: 'Text to insert at the focused field.' },
      },
      required: ['containerId', 'text'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string' },
          inserted: { type: 'number' },
          focused: { type: 'string' },
          landed: { type: 'boolean' },
        },
        required: ['containerId', 'inserted', 'landed'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        const landed = record.landed === true
        return [{
          type: 'text',
          text: landed
            ? 'Typed ' + String(record.inserted) + ' character(s) into <' + String(record.focused ?? 'input') + '> in container ' + String(record.containerId) + '.'
              + nextHint('press Enter with box_press_key to submit, or click the submit control')
            : 'Nothing was focused, so the ' + String(record.inserted) + ' character(s) went nowhere.'
              + nextHint('click the field with box_click_element first, then type again'),
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      if (typeof record.text !== 'string' || record.text === '')
        throw new Error('text is required and must be non-empty')
      const result = await withSession<{ inserted: number, focused: string | null, landed: boolean }>(
        containerId, 'debug_type_text', { text: record.text },
      )
      return { containerId, ...result }
    },
  })

  registerTool(ctx, {
    name: 'box_press_key',
    description:
      'Press and release one named key, for a submit or a keyboard shortcut. Enter submits a '
      + 'form and submits nothing that is not a form, so when a click on the submit control '
      + 'reports landed:false, try Enter here instead. Names: Enter, Tab, Escape, Backspace, '
      + 'Delete, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown, Shift, '
      + 'Control, Alt. An unknown name is an error, never a guess.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container id to press in.' },
        key: { type: 'string', description: 'Key name, e.g. Enter or PageDown.' },
      },
      required: ['containerId', 'key'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string' },
          key: { type: 'string' },
        },
        required: ['containerId', 'key'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        return [{
          type: 'text',
          text: 'Pressed ' + String(record.key) + ' in container ' + String(record.containerId) + '.'
            + nextHint('call box_page_text to see what the page looks like now'),
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const record = asRecord(args)
      if (typeof record.key !== 'string' || record.key.trim() === '')
        throw new Error('key is required')
      const result = await withSession<{ key: string }>(containerId, 'debug_press_key', {
        key: record.key.trim(),
      })
      return { containerId, ...result }
    },
  })

  registerTool(ctx, {
    name: 'box_set_browser',
    description:
      'Choose which browser the other box_* tools drive, and report the one in use. Call it '
      + 'when a page tool fails to launch a browser, or when auto-detection picks a browser you '
      + 'would rather not use. Pass an absolute path to a Chrome or Edge executable to pin it; '
      + 'pass an empty string to go back to auto-detection. The path is validated before it is '
      + 'saved, so a typo is rejected here rather than surfacing later as a failed launch.',
    parameters: {
      type: 'object',
      properties: {
        path: {
          type: 'string',
          description: 'Absolute path to a browser executable, or an empty string to return to auto-detection.',
        },
      },
      required: [],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          available: { type: 'boolean' },
          kind: { type: 'string' },
          path: { type: 'string' },
          configured: { type: 'boolean' },
          configuredPath: { type: 'string' },
          problem: { type: 'string' },
        },
        required: ['available'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        if (record.available === false) {
          return [{
            type: 'text',
            text: 'No browser is available: ' + String(record.problem ?? 'unknown reason')
              + (record.configuredPath ? ' (configured: ' + String(record.configuredPath) + ')' : '')
              + nextHint('pass a browser executable path to box_set_browser, then retry the page tool'),
          }]
        }
        const pinned = record.configured === true
        return [{
          type: 'text',
          text: 'Using ' + String(record.kind ?? 'browser') + ' at ' + String(record.path ?? 'unknown')
            + (pinned ? ' (pinned by you).' : ' (auto-detected).')
            + nextHint('call box_page_text to see what the page looks like now'),
        }]
      },
    },
    async execute(args) {
      const record = asRecord(args)
      const path = typeof record.path === 'string' ? record.path : ''
      // Only send the field when the caller meant to set one; a bare call is a
      // status query, and an empty path is an explicit reset.
      return path === '' && record.path === undefined
        ? getRpc().call<Record<string, unknown>>('debug_browser_status', {})
        : getRpc().call<Record<string, unknown>>('debug_set_browser_path', { path })
    },
  })

  registerTool(ctx, {
    name: 'box_close',
    description:
      'Close the headless browser for a container and release it. Call it when a long debugging '
      + 'session is finished; the next page tool opens a fresh one. Sessions also close on their '
      + 'own, so this is for tidying up rather than for making something work.',
    parameters: {
      type: 'object',
      properties: {
        containerId: { type: 'string', description: 'Container whose browser session to close.' },
      },
      required: ['containerId'],
    },
    timeoutMs: WARM_BUDGET_MS,
    output: {
      schema: {
        type: 'object',
        additionalProperties: false,
        properties: {
          containerId: { type: 'string' },
          closed: { type: 'boolean' },
        },
        required: ['containerId', 'closed'],
      },
      render: (_args, value) => {
        const record = asRecord(value)
        return [{
          type: 'text',
          text: (record.closed === true ? 'Closed' : 'There was no open session to close for')
            + ' the browser of container ' + String(record.containerId) + '.',
        }]
      },
    },
    async execute(args) {
      const containerId = containerIdOf(args)
      const result = await getRpc().call<{ closed: boolean }>('debug_close', { id: containerId })
      return { containerId, ...result }
    },
  })

}
