# Agent tool calls crash in every Container: dsh runs out of two module worlds

**Status:** reproducible, blocks all agent tool use in Containers
**Component:** `dshboxd` host launch (`box-runtime/src/process/env.rs:534`, `box-runtime/src/lifecycle.rs:229`)
**Severity:** high — the agent cannot run a single command in a Container on any Harness at or above `dsh-v0.1.6-alpha.2`

## Summary

A DSH Host started by `dshboxd` runs the Harness **source checkout** through tsx
(`node --import tsx/esm <instance>/harness/apps/cli/src/bin.ts`). That checkout
carries both `src/` and the `lib/` built from it, and `tsconfig.base.json` maps
every first-party package name to `src`. tsx honours those `paths`, so a module
that was itself loaded from `lib/` still resolves its bare imports to `src/` —
while the plugin row loader takes each package's entry from `lib/` (its
`package.json` `exports`). One package therefore becomes **two module instances**,
and the seams between them that are keyed by `Symbol` stop matching.

The seam that bites is `@deepseek-ai/dsh-tools`' scheduler. Every agent tool call
dies about a millisecond after it is appended, with no `tool/result`, which the
web UI renders as a very short "已中断 · N s" — it looks like an interruption, not
a crash.

This is a Harness resolution defect, not a plugin defect (see *Not a plugin
problem* below). It is listed here because **the launch shape is what makes it
reachable in a Container, and Box owns that launch shape** — and there is a
one-argument fix on the Box side.

## Symptom

Session event log, from a Container on `dsh-v0.1.6-alpha.2` (the `turn/end`
arrives **1 ms** after `tool/call`, with a `step/end` in between; the command
arguments are elided):

```json
{"type":"tool/call","seq":55,"time":1790042892544,"data":{"turn":3,"step":1,"callId":"call_…","name":"bash","arguments":"{…}"}}
{"type":"step/end","seq":56,"time":1790042892544,"data":{"turn":3,"step":1}}
{"type":"turn/end","seq":57,"time":1790042892545,"data":{"turn":3,"reason":{"kind":"error","error":{"message":"Cannot read properties of undefined (reading 'prepare')","code":"UNKNOWN"}}}}
```

The turn error carries only the message; the stack is not persisted, which is why
this took a loader-level instrument to pin down.

## Root cause

Two module instances of one package, each creating its own symbol:

```
harness/packages/core/tools/src/index.ts:463
    export const TOOL_RUNTIME_SCHEDULER: unique symbol = Symbol('@deepseek-ai/dsh-tools.scheduler')
harness/packages/core/tools/lib/index.js:2526
    const TOOL_RUNTIME_SCHEDULER = Symbol("@deepseek-ai/dsh-tools.scheduler");
```

Same description, different identity — `Symbol` compares by reference. The
`ToolRuntime` instance is built by whichever row loaded `lib/`, and hangs its
scheduler on the **`lib`** symbol. The consumer is the agent loop, whose import
was rewritten to **`src`**, so it looks the other one up
(`harness/packages/core/agent-loop/lib/index.js`):

```js
13:  import { TOOL_ABORTED_BEFORE_DISPATCH, TOOL_RUNTIME_SCHEDULER } from "@deepseek-ai/dsh-tools";
586: const prepared = await ctx.tools[TOOL_RUNTIME_SCHEDULER].prepare(call.exec);
```

`ctx.tools[TOOL_RUNTIME_SCHEDULER]` is `undefined`, and `.prepare` throws.
Confirmed by instrumenting the host: `ctx.tools` really is a `ToolRuntime`,
`Reflect.ownKeys(ctx.tools)` really contains a `Symbol(@deepseek-ai/dsh-tools.scheduler)`,
and it is not the symbol being used for the lookup.

## Not a plugin problem

* An **empty `DSH_HOME`, no plugins installed at all**, plain upstream
  `dsh-v0.1.6-alpha.2`, launched the same way, logs both instances:

  ```
  [dual] loaded SRC  …/dsh/packages/core/tools/src/index.ts
  [dual] loaded LIB  …/dsh/packages/core/tools/lib/index.js
  ```

* A third-party plugin set cannot influence it either way: the `paths` block names
  only first-party `@deepseek-ai/*` packages, and plugins installed from npm ship
  `lib/` only, so they are single-instance by construction.
* Removing every plugin leaves the failure unchanged; the visible terminal keeps
  working, because only the agent's tool path crosses the symbol seam.

## Why it is Box's to decide

`dshboxd` builds the Host's argv itself and owns its environment, which leaves a
downstream user no supported way to fix the tree at launch:

| Route | Result |
| --- | --- |
| `NODE_OPTIONS=--import <hook>` in the user's shell | never reaches the Host — the daemon spawns `node` directly |
| `.npmrc` with `node-options=` in the instance Harness | dead — pnpm is not in the launch path at all |
| Boxfile directive (`LABEL` / `ADD`) | none exists for launch args or env |
| Daemon environment (`dshboxd.service`) | works — `dsh_host_policy` is not clean-room, so `NODE_OPTIONS` is inherited — but it applies to every Container and to every toolchain child, including in-tree `pnpm run build` |

So a user who hits this has to edit the Container's copy of
`tsconfig.base.json` by hand, after `dshbox run`, once per Container. That is the
workaround in use here, and it is verified: pinning the mappings to `lib` turns
the same session into

```json
{"type":"tool/call","time":1790076476928,"data":{"name":"bash","arguments":"{\"command\":\"echo one-world-ok\"}"}}
{"type":"tool/result","time":1790076477035,"data":{"turn":5,"step":1}}
{"type":"turn/end","time":1790076478023,"data":{"reason":{"kind":"completed"}}}
```

## Proposed fixes, cheapest first

1. **Launch the built entry instead of the source entry.**
   `apps/cli/lib/bin.js` is present in every Container (`dshbox pull template`
   already runs `pnpm run build` to produce the client artifacts, and the build
   output is copied along with the template — verified on the `dsh-v0.1.6-alpha.2`
   and `dsh-v0.1.7-alpha.1` bases). Starting `node apps/cli/lib/bin.js
   --profile web …` puts the process in one world with no `paths` in play and no
   tsx in the hot path. This is the change I would make first; the only thing it
   costs is editing source in a live Host, which a Container is not supposed to do
   anyway.
2. **Let a Container carry launch arguments.** A Boxfile directive (or a
   `dshbox run --node-option/--import`) would give downstreams a supported way to
   supply a resolver hook without touching the daemon environment globally.
3. **Report the layout upstream.** The real defect is that one package answers to
   two entry points. Either the packaged tree should not ship `src/` next to
   `lib/`, or `tsconfig.base.json`'s `paths` and the manifests' `exports` should
   agree. Until then, every tool that runs a Harness checkout through tsx inherits
   this.

## Reproduction

```sh
dshbox pull template github.com/deepseek-ai/deepseek-harness:dsh-v0.1.6-alpha.2
dshbox build ./examples/boxfile-dshell.dsh --name repro
dshbox run repro --name repro
# open the URL, pick a workspace, and send a prompt that forces a tool call:
#   必须用 bash 工具运行 echo hi，然后只回复那个词。
dshbox container logs <id>          # host up; the failure is in the session log
zstdcat ~/.dsh-cache/instances/<id>/profile/sessions/*/session-*/session.v3.jsonl.zstd \
  | jq -c 'select(.type=="tool/call" or .type=="turn/end")'
```

Observed on `dsh-v0.1.6-alpha.2` and `dsh-v0.1.7-alpha.1` (443 and 479 `paths`
entries pointing at `src`, with `packages/core/tools/lib/index.js` present in
both). `dsh-v0.1.6-alpha.1` escapes only because its prepared base has no built
`packages/core/tools/lib` — not because it is fixed.

Reproduces identically without Box, from a plain Harness checkout:
`node --import tsx/esm apps/cli/src/bin.ts web`.

## Notes on the workaround, for whoever hits this first

The rewrite has to be narrow. Mapping everything to `lib` breaks the Host
outright: `@deepseek-ai/cordis` exports `FiberState` from `src` and not from its
`lib`, so the vendored packages must stay on source. A directory mapping must keep
its leading `./` (`tsx` rejects a `paths` target that is neither relative nor
`baseUrl`-rooted, and the Harness sets no `baseUrl`), and "the build exists" has to
mean a regular **file** — a leftover `lib/types/` directory will otherwise pass an
`existsSync` check and produce a resolution that cannot load.
