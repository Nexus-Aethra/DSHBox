# DSH Box — Agent Guide

> Desktop launcher and lifecycle manager for DeepSeek Harness (DSH). Tauri 2
> shell + React management UI + Rust Cargo workspace + `dshboxd` sidecar.

## Repository layout

```
src/                       React/TypeScript management UI (the "Box" UI)
  App.tsx                  Shell; gates mount on dshboxd `ping`
  i18n.ts                  All UI strings — English + 简体中文 (single source)
  main.tsx                 Vite entry
  features/                container-details, resources-page, tasks, toolchains
  shared/{api,types,ui}/   IPC bridge, domain types, cross-feature widgets
  state/                   useContainers / useResources / useSettings / useTasks
  ui/                      Primitive components (Button, Card, Field, ...)
src-tauri/                 Rust workspace + Tauri shell
  src/desktop/app/         Tauri modules: containers, extensions, lifecycle, ...
  src/cli/                 CLI subcommands (build, run, container, template, plugin, bundle, ...)
  crates/                  Framework-free crates (see Architecture below)
  crates/dshboxd/          Background server sidecar
  tools/runtime-packager   Bundled Node/pnpm runtime packager
docs/                      HANDOFF.md, development.md, specs/, design/, notes/
examples/                  Sample boxfile.dsh and plugin-chains demo
scripts/                   Build/prepare scripts and sandbox e2e harnesses
runtime-lock.json          Pinned Node + pnpm integrity for bundled runtime
```

## Build, lint, test

Prereqs: Node 22.13+ with pnpm (the pinned `packageManager` needs `node:sqlite`), Tauri 2 prereqs for your platform, Rust toolchain.

```bash
pnpm install
pnpm runtime:prepare      # fetch bundled Node/pnpm runtime manifest
pnpm server:prepare       # build the dshboxd sidecar
pnpm tauri dev            # dev shell (frontend + Tauri)
pnpm dev                  # frontend only, in a browser — see "Browser debugging" below
pnpm build                # frontend typecheck + vite build (tsc --noEmit && vite build)
pnpm tauri build          # desktop binary (needs `custom-protocol` feature in release)

# Per-platform installers
pnpm bundle:windows       # NSIS .exe (runs scripts/bundle-windows.mjs)
pnpm bundle:linux         # .deb/.rpm
pnpm bundle:macos         # .dmg

# Tests
cd src-tauri && cargo test --workspace            # full Rust suite (160+ passing)
scripts/e2e-*.sh                                   # sandbox-isolated end-to-end
```

The `custom-protocol` Cargo feature (set automatically by `tauri build`)
switches the main window from dev URL `http://localhost:1420` to the embedded
frontend. Manual `cargo build` for release needs it explicitly.

## Architecture rules

### Rust workspace (`src-tauri/crates/`)

| Crate                       | Role |
|----------------------------|------|
| `box-foundation`           | Config, paths, JSON persistence, validation |
| `box-runtime`              | Absolute-path process exec + libgit2 checkout primitives |
| `box-scheduler`            | Persisted background task queue, locks, cancellation |
| `box-state`                | `ResourceStateManager` — primary read model |
| `box-toolchains`           | Bundled Node/npm/pnpm resolver |
| `box-dsh-versions`         | DSH GitHub catalogue + install/remove |
| `box-containers`           | Container metadata + active Host registry |
| `box-extensions`           | Repository plugin/skill scan, copy, export |
| `box-image`                | `.dsh` parser, manifest v6, gzip tar I/O |
| `box-resources`            | Container resource kinds, extraction, injection |
| `box-dsh-context`          | Patch YAML / context snapshot rendering |
| `box-server-core`          | `dshboxd` helpers, service install |
| `box-api`, `box-client`    | IPC + client adapter layer |
| `dshboxd`                  | Sidecar binary (own crate) |

Dependency direction: `foundation/runtime/scheduler/state` → functional crates →
Tauri desktop adapters. **Feature crates must not depend on Tauri or one
another's mutable state.** Only the top-level `dshbox` package depends on Tauri.

Long operations go through `box-scheduler`; Tauri IPC handlers submit tasks,
they don't run them inline.

### Frontend (React)

- All UI strings live in `src/i18n.ts`. When adding UI copy, add to **both** `en`
  and `zh` blocks in the same file. Use the `Language` type from
  `shared/types/domain.ts`.
- IPC is funneled through `src/shared/api/box-api.ts`. Don't call `invoke`
  directly from feature code.
- Data hooks live under `src/state/`. Pages and panels consume hooks; pages do
  not own task/polling logic.
- Three top-level sections: `Container`, `Resources`, `Settings`. Navigation
  lives in `App.tsx`.
- The Box UI is intentionally separate from DSH's own client: white, minimal,
  one primary action per view, restrained neutral palette. Do not imitate the
  DSH UI.
- A startup gate in `App.tsx` waits for `dshboxd` `ping` before mounting
  features — data hooks always run against a ready daemon.

## Naming and product names

- Binaries: `dshbox` (CLI + desktop), `dshboxd` (sidecar), `dsh-box` (legacy
  paths and `~/.dsh-box/` config dir still in use).
- Box data lives under the user-selected runtime directory; small machine-local
  config lives under `~/.dsh-box/`. DSH runtime data, plugin dependency trees,
  and pnpm stores live below the selected runtime dir — **never** in `~/.dsh-box`.
- The extension repository (`repository/`) is independent of Containers.
  Importing copies into a Container; later repo edits do not mutate installed
  copies.

## Browser debugging

`pnpm dev` serves the Box UI in a plain browser against the **running daemon**, so
the UI can be inspected with browser devtools instead of a webview. `scripts/dev-rpc-bridge.mjs`
adds a dev-only `/__rpc` route (`apply: 'serve'`, never in a production build) that
forwards `box-api.ts` calls to `dshboxd` over its loopback RPC. Start the daemon
first (`dshboxd`, or any CLI command that spawns it) and set `DSHBOX_CONFIG_DIR` if
the runtime directory is not the default.

The bridge covers the read-only surface — config, templates, containers, tasks,
toolchains, and `plugin_dependency_graph`. Commands that the desktop layer
orchestrates rather than the daemon (anything that enqueues a scheduler task, drives
the process lifecycle, or opens a native dialog) answer with an explicit "not
available in browser dev mode" error instead of a stub, so a missing capability is
obvious. `listenTask` degrades to a no-op and task progress comes from the 3s poll.

## Known gotchas

- **No system Node/pnpm/Git required (Windows); Linux uses host git with isolated
  config.** Bundled Node + pnpm (pinned in `runtime-lock.json`, SHA-256/SHA-512
  verified at build time) is invoked via absolute paths. On Windows the bundled
  Git (PortableGit) is shipped inside `<runtime>/git/` and the clean-room policy
  prepends it to `PATH`. On Linux, where no bundled git is published yet, the
  daemon falls back to the host's `git` binary via a clean-room host-passthrough
  mode — the binary directory is added to `PATH` but `HOME`, `GIT_CONFIG_GLOBAL`,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, and `XDG_CONFIG_HOME` are
  redirected to `<storage>/git/...` so the host `~/.gitconfig` cannot leak.
  Windows never falls back; install Git for Windows manually if needed. libgit2
  still handles DSH's own clones — never shell out to `git`.
- **DSH web server is loopback-only** (`127.0.0.1`, dynamic port) and requires
  a per-launch capability token from the shell for launcher-only endpoints.
  WebView navigation must stay on that loopback origin.
- **Proxy env is poison for loopback + host spawn.** Never let `HTTP_PROXY` /
  `HTTPS_PROXY` / `ALL_PROXY` (any case) reach (a) dshboxd's own loopback
  probes — use a reqwest client with `.no_proxy()` — or (b) the spawned DSH
  host process — `dsh_host_policy` (`box-runtime/src/process/env.rs`) strips
  all 8 proxy aliases. A host that inherits a proxy self-terminates with an
  "opening the default browser" error.
  (c) the DSH front webview: WebKitGTK resolves proxies through GIO, and a
  system proxy hands the host's `127.0.0.1:<port>` URL to the proxy (GNOME's
  resolver matches a literal `localhost` in `ignore-hosts`, but not `127.*` or
  `127.0.0.1`), leaving the window blank. `main.rs` pins
  `GIO_USE_PROXY_RESOLVER=dummy` on Linux for that reason; env vars such as
  `no_proxy` do **not** affect this path. Windows uses `--no-proxy-server` in
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` for the same failure.
- **Plugin lifecycle scripts are user-approved code execution.** Do not relax
  pnpm supply-chain checks (lifecycle-script approval, minimum-release-age).
  The `dshbox.allow-build` LABEL in a boxfile authorizes the **top-level
  source only**; the daemon auto-derives transitive `package@version` keys
  from pnpm's `[ERR_PNPM_IGNORED_BUILDS]` /
  `[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED]` markers and retries (multi-round
  loop in `dshboxd/src/sealed.rs`, `add_pnpm_build_approval` is an idempotent
  YAML merge producing a single bare-scalar `allowBuilds:` section).
- **Runtime archive integrity.** Verify SHA-256 (Node) and SHA-512 (pnpm)
  before use; a failed verification aborts startup, no silent fallback.
- **DSH ≥ `dsh-v0.1.5-alpha.1` cannot be built from source on Linux/macOS with
  the bundled runtime.** `pnpm run build` now runs `build:native-system` first
  (`harness/scripts/build.ts`), which compiles the `flock` Node-API addon and
  requires `<node>/include/node/node_api.h`. `is_redundant_node_file` in
  `tools/runtime-packager` strips `include/` as "never used", so the container
  prepare aborts with `Node-API headers missing at …`. Windows is unaffected
  (`native/system/scripts/build.ts` exits 0 for `--host-addon-only` there).
  Verified 2026-09-17 against `0.1.6-alpha.1` (commit `0d1f500`): restoring the
  headers by hand makes the same container build and start, so this is the only
  blocker — but it also needs a host C compiler, which the runtime bundle
  otherwise never requires.
- **Prepared/sealed templates.** Pulling a root Harness template prepares a
  complete source tree (`pnpm install` only — `validate_prepared_harness`
  checks that tree; the frontend build happens later, when a container is
  prepared from it at `dshboxd/src/sealed.rs` "Building DSH frontend"). `dshbox build`
  copies that base and publishes a sealed physical template with locally packed
  plugin artifacts installed. Container creation copies that sealed tree;
  Container startup must never install or build DSH. `dshbox image` remains a
  deprecated alias forwarding to `build`/`template`. The authoritative design
  is `docs/specs/prepared-template-runtime.md`.
- **Container resources are user state, not code.** `box-resources` moves chat
  history, credentials and plugin state between containers. Injection refuses an
  existing destination unless `--overwrite`/`--merge` is given and refuses a
  running container unless `--restart` is; a secret kind's payload is `0600` in
  the store and at the destination; a path never leaves the container
  (`safe_join` rejects `..` and absolute paths before normalization, and a
  symlink pointing outside the source is refused, not followed). Plugin
  declarations (`package.json` → `dshbox.resources`) are trusted; paths scanned
  out of a plugin's code are candidates the user confirms. Extracted records go
  through the document store; payloads stay in `<runtime>/resources/<id>/`.
- **Plugin cache dedup.** A second `build` of the same `name+version` should
  hit the existing hash entry (`<root>/repository/plugins/img-<id>/source/`)
  and not produce a duplicate `img-…` row (see
  `docs/notes/2026-08-17-bugs-plugin-cache-and-template-not-found.md`).
- **Template resolution.** Built templates' `list.json` must be resolved via
  `lookup_template_path`; do not hardcode `script.dsh` (legacy flat-file path).
- **Workspace extension scan** detects plugins/skills under
  `<container>/workspace` for the UI to import into the repo (recently added;
  review alongside related changes before committing).
- **DSH update flow** is atomic: download → verify → health-check → switch
  `current` → install bridge → restart. On any failure, keep current runtime
  and retain the failed diagnostic log.
- **Tauri deb/rpm bundler dereferences symlinks.** The bundled runtime must
  ship plain executable shims (not symlinks) for `npm`/`npx`/`corepack` — see
  `tools/runtime-packager`. Dev runs are unaffected; only installed packages
  break if this regresses. Tracked children spawn with `setsid` pre_exec so
  kill_tree can't take down unrelated process groups.
- **`src-tauri/dist/` is generated** by Vite before the Tauri build and
  bundled into the desktop binary — never edit files under it. It must stay the
  only frontend output: `vite.config.ts`'s `outDir` and `build.frontendDist`
  (which Tauri resolves against `src-tauri/`) have to agree, and no build step
  may copy a second `dist/` over it. `build.rs` used to mirror the repo-root
  `dist/` in, which silently shipped a months-old frontend — a stale bundle
  looks exactly like "the feature was never implemented".

## Docs to read before touching sensitive areas

| Area | Read |
|------|------|
| Architecture, runtime model, milestones | `docs/HANDOFF.md`, `docs/development.md` |
| Built template design (image pivot) | `docs/specs/image-build.md` |
| Template system behavior | `docs/template-system.md` |
| Plugin pnpm install flow | `docs/design/pnpm-managed-plugin-install.md` |
| Recent bugs / partial fixes | `docs/notes/2026-08-17-bugs-plugin-cache-and-template-not-found.md` |
| Linux host-git passthrough / Windows pnpm base | `docs/notes/2026-08-21-*.md` |
| Release handoff snapshot | `handoff.md` (repo root) |

## Conventions quick-reference

- Rust: edition 2021, Cargo workspace with `resolver = "2"`. Prefer
  framework-free functions over Tauri-coupled types inside crates.
- Frontend: TypeScript strict, React 18, Vite 6, no extra UI library — build
  primitives under `src/ui/`.
- **Persistence: document store, not scattered files.** Persisted indexes go
  through `box_foundation::collection::DocumentStore` (SQLite backend in
  `box-store` at `<runtime>/state/dshbox.db`; JSON backend only as legacy
  import source and fallback). Domain code holds a typed `Collection<T>` and
  never touches SQL; upserts never delete (daemon + desktop share the store),
  deletion goes through `delete_document`. Legacy per-domain JSON files
  migrate on first open via `LEGACY_SCOPES` in `box-store`, then are renamed
  `*.pre-sqlite`. Content-addressed directories (`templates/<hash>/`,
  `data/<digest>/`, `runtimes/`) stay on disk. `config.json` stays in
  `~/.dsh-box/` — machine-local, never enters the store. Schema changes are
  forward-only migrations gated by `PRAGMA user_version` — no downgrade path.
- Errors: surface them — Box keeps failed diagnostic logs and a recovery view
  rather than silently falling back.
