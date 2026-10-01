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

## What the plugin layer adds

Four tools over the above, whose descriptions are written as instructions
rather than mechanism summaries:

  box_page_text    the recommended first call; renders the page as text
  box_scroll       reports and changes scroll extent
  box_click_element  verifies the click landed on the intended element
  box_click_at       same, by coordinate
  box_type_text      types into the focused field and reports focus
  box_press_key      presses a named key

box_page_text must open with an explicit instruction to call it first, and
every tool response carries a next field naming the natural follow-up call,
so an agent that has never seen these tools is oriented by a single response.
