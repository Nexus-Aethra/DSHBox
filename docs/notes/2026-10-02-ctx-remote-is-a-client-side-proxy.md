# ctx.remote is a client-side proxy, not a host API

Found 2026-10-02 while trying to create a workspace from a host plugin without
a native dialog.

`@deepseek-ai/dsh-api-workspace-controller` documents its service as "Host
service backing the generated `ctx.remote.workspace` namespace", and that reads
like a host API. It is not. Every call site of `ctx.remote.*` in the harness is
under `packages/client/ui-*`:

    client/ui-workspace/src/client/index.ts:119   ctx.remote.directoryPicker
    client/ui-reference/src/client/index.ts:71    ctx.remote.fileReferences
    client/ui-settings-account/...                 ctx.remote.userQuestions

The host *serves* the namespace; the client calls it. `ctx.remote` does not
exist on a host `Context`, so a host plugin reading `ctx.remote.workspace`
fails with "cannot get property of undefined" -- which is exactly what the
agent hit when it called the tool.

## Consequence

Registering a workspace by path without a file dialog is possible, but it is
not a host-side change. The `create({ path })` command lives behind the client
half, so the package has to ship a browser export:

    "dsh": { "client": { "platform": "web", "inject": ["client"] } }
    "exports": { "./client": { "default": "./dist/client.mjs" } }

discovered from `dsh.client` by the node half of `@deepseek-ai/dsh-client-modules`
(`parseDshClient`; `platform` is required, `inject`/`external`/`immediately`
optional), composed into `window.__DSH_BOOT__`.

Also worth knowing: `dshClient` appears in that package's doc comments but no
package in the harness uses it. The real key is the nested `dsh.client`.
