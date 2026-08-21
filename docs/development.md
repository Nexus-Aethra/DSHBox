# DSH Box development guide

## Purpose

DSH Box is a Tauri desktop launcher, CLI, and `dshboxd` sidecar for DeepSeek
Harness (DSH). The Box management UI is a separate React application; it starts
DSH on loopback and displays the DSH web UI in a WebView. It does not modify or
imitate DSH's bundled client.

## Current runtime model

The runtime root uses the prepared/sealed model:

```text
runtime-root/
  staging/<task-id>/                 # task-private and disposable
  repository/plugins/<digest>/       # metadata + immutable artifact.tgz
  templates/base-<digest>/harness/   # prepared Harness source/dependency cache
  templates/sealed-<digest>/         # source recipe, without node_modules
  instances/container-<id>/          # independent runnable copy
  state/                             # schema-10 state, resources, tasks
  logs/
```

Pulling a root Harness template clones to staging, runs bundled `pnpm install`,
validates the dependency cache, then atomically publishes a prepared base.
Building a Boxfile copies source without `node_modules` and records local plugin
tarballs in a sealed recipe. Creating a container copies that recipe to its final
path, runs offline install, adds artifacts, and builds the frontend. Starting a
prepared container only runs bundled `pnpm dsh web` from its local Harness directory.

Do not introduce a launch-time dependency install/build, a shared mutable
Harness checkout, workspace-path injection, or a symlink/junction from a
container into the repository/template tree. The complete contract and
migration policy are in [prepared-template-runtime.md](specs/prepared-template-runtime.md).

## Code modules

The Rust side is a Cargo workspace. Only `src-tauri/`'s top-level `dshbox`
package depends on Tauri. It owns windows and IPC adapters; business behavior
lives in framework-free crates.

```text
box-foundation  paths, config, JSON persistence, validation
box-runtime     absolute-path process execution and libgit2 checkout
box-scheduler   task records, locks, cancellation, recovery
box-state       ResourceStateManager read model
box-toolchains  bundled Node/npm/pnpm resolution
box-containers  container metadata and host registry
box-extensions  repository plugin/skill import and artifact handling
box-image       .dsh parser and template manifest handling
dshboxd         scheduler-backed lifecycle and HTTP RPC
dshbox          Tauri shell and CLI adapters
```

Dependency direction is `foundation/runtime/scheduler/state` → functional
crates → desktop adapters. Feature crates must not depend on Tauri or on one
another's mutable state. Long work is submitted to `box-scheduler`; Tauri
handlers and CLI handlers do not execute it inline.

## Toolchains and process execution

No system Git, Node, npm, or pnpm is required. libgit2 performs checkouts
that DSH itself owns; the release bundle carries the integrity-verified
Node, pnpm, and Git versions pinned by `runtime-lock.json`. Windows ships
Git-for-Windows PortableGit; Linux builds a CI-produced private bundle.
Always invoke them through the resolver's absolute paths, with a
task-specific environment and working directory. Never use a global pnpm
store for a published tree.

The clean-room package-manager policy prepends `<runtime>/git/cmd` (or
`bin`) to `PATH` and pins `GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=<storage>/git/config/global.gitconfig`, and
`GIT_TERMINAL_PROMPT=0` so the host's `~/.gitconfig` (or registry-backed
Git config) cannot leak into pnpm children. Authentication for Git
sources is unsupported in this release; only public HTTPS is allowed.

Allocate a loopback port immediately before spawning a DSH host. Bind only
`127.0.0.1`, pass a per-launch capability token, keep WebView navigation on the
local origin, and retain diagnostic logs for all preparation, build, and host
failures.

## UI and API rules

- UI strings belong in both `en` and `zh` sections of `src/i18n.ts`.
- Feature code uses `src/shared/api/box-api.ts`, not direct Tauri `invoke`.
- Hooks under `src/state/` own polling and task subscriptions.
- CLI and desktop actions submit the same daemon task types and expose the
  same stages: prepare base, import artifact, seal template, copy container,
  launch host, ready/failed.

## Development commands

```bash
pnpm install
pnpm runtime:prepare
pnpm server:prepare
pnpm tauri dev
pnpm build
pnpm bundle:windows
cd src-tauri && cargo test --workspace
```

Use a disposable selected runtime root for end-to-end tests. Schema 10 is
forward-only: tests must not silently consume a legacy root. Cover both Linux
and Windows with a pull → build → create → start flow, including one template
with the locally cached DSH-better-sidebar artifact.
