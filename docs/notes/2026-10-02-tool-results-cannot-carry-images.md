# A tool result cannot carry an image to the model

Found 2026-10-02, finishing the page-debugging plugin's end-to-end run.

    agent: "I cannot tell from the text result what colour the top bar is,
            because the screenshot image itself is not available to me here."

Everything on the way there worked. The tool call ran, the RPC captured a PNG,
`attachments.saveImage` accepted it, and the render returned both a text block
and an image block. The text arrived. The image did not.

It is not the tool's shape. `ImageBlock` in `@deepseek-ai/dsh-llm` is

    type: 'image'
    attachment: ImageAttachmentRef

-- which is exactly what the tool emits, and its own doc comment says:

    The block is deliberately role-neutral; assistant-side rendering is forward
    compatibility -- the current production adapters declare text-only output,
    so only user messages may carry images.

A tool result is assistant-side. So an image in one is dropped by design. There
is no render shape, framing or ordering that gets around it: `render` returns
`ContentBlock[]` (confirmed against shipped tools, which all return
`[{ type: 'text', text }]`), and `ImageBlock` is the only image variant.

## What follows

`box_screenshot` still has a use -- leaving an artifact a human looks at, and
recording something the page text cannot express. What it must not do is imply
the model will see it, so its description now says so and points at
`box_page_text` for actually learning what is on a page.

Anyone expecting a browser-automation tool to show a model a screenshot should
know this before they build on it. The gate is the adapter's text-only output
declaration, not the plugin.
