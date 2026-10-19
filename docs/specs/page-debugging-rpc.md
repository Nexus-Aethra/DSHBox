# Page-debugging RPC contract

Normative contract between dshboxd (Rust) and the dsh-box-sandbox plugin (TypeScript).
Both sides implement against this file; neither invents its own shapes.

Every method takes the session id in the request field named id. Every method
may open a session on demand when the id is a known container, matching the
existing debug_open / debug_close behaviour.

## The design problem this solves

A screenshot answers what a page looks like. A selector query answers what
matches one expression. Neither answers what is on the page, which is the
question an agent driving a UI actually asks, and which today costs a dozen
round trips of guesswork. Worse, a click that lands on the wrong element still
reports success, so an agent cannot tell a working step from a no-op.

So the contract has two rules that override convenience:

1. Never make the agent remember. Anything a decision depends on travels in
   the response, including scroll extent, viewport position and what is
   off-screen. An agent with a short working memory must still be correct.
2. Never report an unverifiable success. A mutating call reports what it
   observed, not what it attempted.

## debug_page_text

Renders the page as structured text from the accessibility tree.

Request: id, optional limit (default 200, clamp 1..1000), optional role
(case-insensitive substring match on the role).

Response:
  count       number of entries returned
  truncated   true when the limit cut the list
  belowFold   how many entries are outside the viewport
  elements    array of:
                role       accessibility role, never empty
                name       accessible name, never empty
                value      present only when non-empty
                x, y       viewport centre of the element
                width      integer pixels
                height     integer pixels
                inViewport true when the rect intersects the viewport
                clickable  true for roles that accept a click

Implementation: one Accessibility.getFullAXTree call, drop ignored nodes and
structural noise (none, presentation, generic, StaticText, LineBreak,
Separator, ScrollBar, ScrollArea, RootWebArea), drop entries with neither name
nor value, cap at limit, then one DOM.getBoxModel per surviving node for the
rect. The role is what a screen reader perceives, which is what an agent
should act on, not a DOM tag.

## debug_scroll

Reports how far the document extends, and optionally scrolls it.

Request: id, optional to (absolute CSS pixels). Omit to only report.

Response:
  scrollTop      current offset
  scrollHeight   full document height
  viewportHeight visible height
  screensBelow   how many viewports remain below the fold, rounded up
  moved          true when a scroll was requested and performed

Implementation: one Runtime.evaluate against document.scrollingElement.

## debug_click_at and debug_click_element

Unchanged inputs. The response gains verification:

  landed      true when the click was dispatched onto the intended target
  hitTag      tag of the element actually under the point
  hitText     short text of that element
  occludedBy  when landed is false, what covered the target instead

Implementation: before dispatching, one Runtime.evaluate of
document.elementFromPoint(x, y). For click_element the intended node is
compared against that hit (a hit is acceptable when it is the target, a
descendant, or an ancestor). A mismatch means something covered the target
and the click must be reported as not landed, naming the cover.

## debug_type_text

Request: id, text (non-empty).
Response: inserted (character count), and focused (the tag of the element
that held focus, or null when nothing editable had it).

The focused field is the whole point: input that lands nowhere is a silent
no-op, and the caller cannot otherwise tell it from a success. A missing
focused target is reported rather than counted as inserted.

## debug_press_key

Request: id, key (named key; unknown names are an error, never a guess).
Response: key (the canonical CDP spelling).

## The methods added after this file was first written

These joined the same session contract and are not described above. All take
`id` like every other method, except where noted.

### debug_open, debug_close

Open and release a headless Chrome for a container, and report the viewport in
use. `debug_open` takes optional `width` and `height` to set the window at
launch; without them the default is 1600x1200. A session is per container and
opens on demand, so a caller need not open before it can read -- but closing is
worth doing, because the profile is only released then.

### debug_set_viewport

Resize a live session: `Emulation.setDeviceMetricsOverride`, then
`Page.getLayoutMetrics` to report the size actually in effect. The reply carries
`clamped`, because a request outside 320..7680 x 240..4320 is honoured as far as
the browser allows, and a caller that assumed otherwise would compute positions
from a viewport that is not the one on screen. `debug_browser_status` reports
which browser is in use and is worth reading first when a launch fails, since
the answer is often a browser installed somewhere other than where it was
expected; `debug_set_browser_path` pins it.

### debug_click_by_name

Click by the role and name `debug_page_text` reported -- the one path that
cannot hit the wrong control, since a CSS selector is a guess and a guess that
happens to match still reports success. Takes optional `within` to scope the
search when the same name appears in a dialog and the page behind it.

### debug_query_elements

A narrow CSS-selector query, kept for what `debug_page_text` cannot express.
The plugin exposes no tool for it: `debug_page_text` covers what an agent
actually asks, and a second listing is a second thing to choose between. The
RPC remains, because the UI and a future caller may want it.

### Container workspace and URL methods

Not page methods, but on the same session and sharing the `id` convention, and
needed before a caller can open anything: `container_url` (the running host's
authenticated loopback URL), `browse_container_paths`, `container_url_probe`,
`list_container_workspaces`, `add_container_workspace`,
`remove_container_workspace`.

## What the plugin layer adds

Nine page tools, whose descriptions are written as instructions rather than
mechanism summaries:

  box_page_text       the recommended first call; renders the page as text
  box_scroll          reports and changes scroll extent
  box_click_element   verifies the click landed on the intended element
  box_click_by_name   the same, by the role and name the listing reported
  box_click_at        the same, by coordinate
  box_type_text       types into the focused field and reports focus
  box_press_key       presses a named key
  box_screenshot      stores a PNG; the image does not reach the model
  box_close           releases the browser

Eleven more cover the box itself (`box_lifecycle`, `box_overview`,
`box_resources`, `box_build`, `box_workspace`, `box_set_viewport`,
`box_task`, `box_plugins`, `box_create`, `box_templates`, `box_settings`).
83 of the daemon's 91 methods are reachable from one of them.

`box_page_text` opens with an explicit instruction to call it first, and every
response names the natural follow-up, so an agent that has never seen these
tools is oriented by a single response.

Three things the wrapping absorbs, because the daemon is not consistent about
them and a caller should not have to be:

- **One argument name for the container.** The daemon spells it `id`,
  `containerId` or `container` depending on the method. A tool takes
  `containerId` for every action and maps it per call, because a caller that
  guesses once is right for most verbs and silently wrong for the rest.
- **Enqueue and wait together.** Most of what an agent does to a box is a
  background task, not a call. The tools enqueue and poll together and return
  the finished record, so a caller never writes a polling loop -- and never
  reports a task that is still running as though it had finished.
- **Task states are lower case on the wire.** `TaskState` is declared with
  `#[serde(rename_all = "lowercase")]`, so what crosses is `succeeded`,
  `rolledback`, `cancelled`. The Rust spelling matches none of them. A
  `failed` task is finished when the record carries `finishedAt`, which the
  scheduler stamps once the task *and its rollback* are done -- a task still
  rolling back has none, and is correctly still waited on.

One consequence worth writing down, because it looks like a broken tool and is
not: a rebuilt bundle copied into a running container's `node_modules` changes
nothing, because the host loaded the plugin when it started. A call that
returns the previous build's error message is measuring the previous build.
