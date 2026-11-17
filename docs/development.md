# DSH Box development guide

## Purpose

DSH Box is a Tauri desktop launcher, CLI, and `dshboxd` sidecar for DeepSeek
Harness (DSH). The Box management UI is a separate React application; it starts
DSH on loopback and displays the DSH web UI in a WebView. It does not modify or
imitate DSH's bundled client.

## System boundary

A React management UI, a Tauri shell/CLI, and the `dshboxd` sidecar. The daemon
is the sole writer of resource state and the only owner of the scheduler: the
desktop and the CLI submit the same RPC-backed work and consume the same task
and resource events, and neither keeps a second lifecycle implementation of its
own.

```text
React UI / dshbox CLI
          |
          v
 Tauri adapters / HTTP client
          |
          v
 dshboxd: scheduler + state + lifecycle
          |
          +-- libgit2 checkout
          +-- bundled Node/pnpm process execution
          +-- resource and diagnostics persistence
```

Only the desktop package depends on Tauri. Crate dependencies flow one way,
`apps → edge → domain → core`: a crate in `core/` cannot reach business code, and
`domain/` sees only `core` and itself. The Tauri package uses the three lower
layers but never a binary under `apps/`. `scripts/check-layering.mjs` enforces
this (see "Code modules" below).

## Current runtime model

The runtime root uses the prepared/sealed model:

```text
runtime-root/
  harness-mirror.git/                # shared bare cache of upstream Harness objects
  staging/<task-id>/                 # task-private and disposable
  repository/plugins/<digest>/       # metadata + immutable artifact.tgz
  templates/base-<digest>/harness/   # prepared Harness source/dependency cache
  templates/sealed-<digest>/         # source recipe, without node_modules
  instances/container-<id>/          # independent runnable copy
  state/                             # schema-10 state, resources, tasks
  logs/
```

Pulling a root Harness template fetches the requested version into
`harness-mirror.git` (shallow, so a new version transfers only what it adds),
checks that version out of the mirror into staging, runs bundled `pnpm install`,
validates the dependency cache, then atomically publishes a prepared base.
Building a Boxfile copies source without `node_modules` and records local plugin
tarballs in a sealed recipe. Creating a container copies that recipe to its final
path, runs offline install, adds artifacts, and builds the frontend. Starting a
prepared container only runs bundled `pnpm dsh web` from its local Harness directory.

The mirror is the one exception to "no shared state below the runtime root", and
it is deliberately narrow: it holds upstream git objects, it is never checked out
in place, never copied into a template or container, and it is not part of any
resource's addressing — prepared bases are still addressed by digest alone.
A prepared base's Harness tree carries no `.git` at all: the checkout borrows the
mirror's object database through a git alternate, which records an absolute path
into a cache the user may purge, so the caller removes it before publishing. It
is a cache, not user data: `dshbox template mirror purge` deletes it and the next
pull rebuilds it, and `prune_orphaned_data` does not scan it.

Do not introduce a launch-time dependency install/build, a shared mutable
Harness checkout, workspace-path injection, or a symlink/junction from a
container into the repository/template tree. The complete contract and
migration policy are in [prepared-template-runtime.md](specs/prepared-template-runtime.md).

## Persistence and publication

`~/.dsh-box/` holds machine-local settings only — `config.json`, notably which
runtime root is selected. Everything large lives below that root, and
`<runtime>/state/` holds the indexes: `dshbox.db` is the document store (task
queue, resource records, resource views), and `resource-map.json` records which
sealed templates, plugin artifacts and containers still reference each other.
That map is what gates removal and lets the scheduler clean up in the
background without deleting something still in use. Content-addressed
directories (`templates/<digest>/`, `data/<digest>/`, `runtimes/`) stay on disk
as directories, not as store rows.

Every long operation writes into a task-private `staging/<task-id>/` directory,
validates what it produced, then atomically renames it into the published path
and commits the record. A failure therefore cannot leave a visible half-built
base, template, artifact or container, and it must not mutate one that has
already been published. Schema changes are forward-only migrations gated by
`PRAGMA user_version`; per-domain JSON files written by older versions are
imported on first open and archived as `*.pre-sqlite`.

## Code modules

The Rust side is a Cargo workspace. Only `src-tauri/`'s top-level `dshbox`
package depends on Tauri. It owns windows and IPC adapters; business behavior
lives in framework-free crates.

```text
core/
  box-foundation       paths, config, document-store contract, JSON, validation, logging
  box-api              wire types shared by the daemon, the desktop and the CLI
  box-store            SQLite document-store backend + legacy JSON import
  box-scheduler        task records, locks, cancellation, recovery
  box-runtime          absolute-path process execution and libgit2 checkout
domain/
  box-toolchains       bundled Node/npm/pnpm resolution
  box-dsh-versions     Harness release catalogue, install/remove
  box-containers       container metadata and host registry
  box-extensions       repository plugin/skill scan, import, export
  box-image            .dsh parser and template manifest handling
  box-resources        container resource kinds, extraction, injection
  box-plugin-graph     cordis service graph, read from source and lockfiles
  box-template-core    template resources on top of the state read model
  box-state            ResourceStateManager, resource map, deletion queue
  box-updates          release discovery, manifest verification
  box-browser          headless Chromium and DevTools sessions
edge/
  box-server-core      dshboxd helpers and user-service install
  box-client           RPC client used by the desktop and the CLI
apps/
  dshboxd              sidecar binary: scheduler-backed lifecycle + HTTP RPC
  box-helper           node agent: runs operations on a worker machine
dshbox                 Tauri shell and CLI adapters (the only Tauri-dependent package)
```

Dependency direction is one-way and enforced by
[`scripts/check-layering.mjs`](../scripts/check-layering.mjs) (`pnpm
layers:check`, run by every packaging path) : `apps → edge → domain → core`. The rationale, the measured
fan-in that decided which crates were merged away, and the rules that are not
layer-shaped live in
[the layering design](design/2026-11-08-node-link-session-and-crate-layering.md)
sections 12-19. `core` never reaches business code, `domain` sees only `core` and
itself, and the Tauri package uses the three lower layers but never a binary
under `apps/`. Because the guard reads the graph from `cargo metadata`, every
manifest spelling is covered, and a workspace member that is neither one of the
four layers nor a build tool under `tools/` is a violation in its own right —
whether or not it happens to depend on something. Feature crates must not depend
on Tauri or on one another's mutable state. Long work is submitted to `box-scheduler`; Tauri
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
