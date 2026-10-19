# Do the box_* tools need merging?

Asked after the plugin was finished: can the toolset be reduced to lower what an
agent has to choose between? Re-checked once every tool had a description that
survives scrutiny, and after `box_query_elements` had already gone (10 -> 9).

The test is not "are two tools similar". It is: **does this tool's own stated
purpose survive once the others are read?** A tool that points at a sibling as
the better answer takes a slot in the model's choices without a way to pick
correctly, and that is the cost. A tool whose purpose nothing else covers is
not a near-duplicate, however much its name resembles another's.

## What is irreducible

| Tool | Why it stays |
|------|--------------|
| `box_page_text` | The only listing. Every other tool is reachable from its output, and its output cannot be reconstructed from anything else. |
| `box_click_element` | Clicking a named control. Now takes role+name *or* a selector, so it absorbed the old query tool. |
| `box_type_text` | Writing. Nothing else types. Reports what held focus, so a value that landed nowhere is a failure rather than a silence. |
| `box_press_key` | Enter, arrows, Escape. `box_type_text` cannot submit a form -- Enter is not a character -- and a click on a submit that reports `landed: false` is answered here. |
| `box_set_browser` | Low frequency, but without it a failed launch is unfixable. It also *reports* which browser is live, which is the first thing to check when a page tool misbehaves. |

## The two that could go, and why I kept them

**`box_click_at`.** Its description already says "Prefer box_click_element
whenever the page listed a name for it", which is close to a self-report that it
is the weaker path. It earns its place for exactly one case: a target with no
accessible name, which `box_click_by_name` cannot resolve. Across the whole
build -- including controls missing from the accessibility tree, which now
resolve through the rendered-document fallback -- that case never came up. But
removing it removes the only way to hit an unnamed control, and the fallback is
text-based: an icon-only button with no text would have nothing to match. The
slot is worth one coordinate tool that a caller will rarely use and can always
ignore.

**`box_close`.** Once a debug session started outliving its container, closing
stale sessions became automatic (`close_sessions_of_stopped_containers`), and
that removed the failure this tool was invented to prevent. What is left is
"free the memory now" -- real, minor, and the description already admits it is
tidying rather than a way to make something work. It is the weakest tool in the
set. Kept, because one slot for a guaranteed clean shutdown is cheap, but it
is the first to go if the count ever matters more than the safety.

## The one that is not about redundancy

**`box_screenshot`.** Not a near-duplicate of anything, and not removable on
overlap. It is the only tool whose output the model provably cannot use: a
tool result is assistant-side content and the production adapters are
text-only, so the model receives dimensions and nothing else. See
`2026-10-02-tool-results-cannot-carry-images.md`.

So the question there is not "what does it overlap" but "what is it for, now
that it cannot show a picture". Two defensible answers: keep it, because it
still leaves an artifact a human looks at and can record something page text
cannot express; or drop it, on the grounds that for a model -- its intended
caller -- `box_page_text` already answers the question it is usually reached
for. Its description now states the limit rather than implying a picture.

That one is a trade-off, not a fact, and it is the user's to make.

## Where this leaves the set

Nine tools, one hub, and every description names what to reach for instead. The
chain an agent actually walks is `box_page_text` -> `box_click_element` ->
`box_type_text`, with `box_scroll` for the fold and `box_press_key` to submit.
The rest are named fallbacks for named situations. Removing more would take one
slot per escape hatch, and the evidence says the escape hatches are what made
this work on a real application rather than only on a tidy one.
