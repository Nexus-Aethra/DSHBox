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

Prereqs: Node 20+ with pnpm, Tauri 2 prereqs for your platform, Rust toolchain.

```bash
pnpm install
pnpm runtime:prepare      # fetch bundled Node/pnpm runtime manifest
pnpm server:prepare       # build the dshboxd sidecar
pnpm tauri dev            # dev shell (frontend + Tauri)
pnpm build                # frontend typecheck + vite build (tsc --noEmit && vite build)
pnpm tauri build          # desktop binary (needs `custom-protocol` feature in release)

# Per-platform installers
pnpm bundle:windows       # NSIS .exe (runs scripts/bundle-windows.mjs)
pnpm bundle:linux         # .deb/.rpm
pnpm bundle:macos         # .dmg

# Tests
cd src-tauri && cargo test --workspace            # full Rust suite (107 passing)
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

## Known gotchas

- **No system Node/pnpm/Git required.** Bundled Node + pnpm (pinned in
  `runtime-lock.json`, SHA-256/SHA-512 verified at build time) is invoked via
  absolute paths. libgit2 handles clones — never shell out to `git`.
- **DSH web server is loopback-only** (`127.0.0.1`, dynamic port) and requires
  a per-launch capability token from the shell for launcher-only endpoints.
  WebView navigation must stay on that loopback origin.
- **Plugin lifecycle scripts are user-approved code execution.** Do not relax
  pnpm supply-chain checks (lifecycle-script approval, minimum-release-age).
- **Runtime archive integrity.** Verify SHA-256 (Node) and SHA-512 (pnpm)
  before use; a failed verification aborts startup, no silent fallback.
- **Prepared/sealed templates.** Pulling a root Harness template prepares a
  complete source tree (`pnpm install` + `pnpm run build`). `dshbox build`
  copies that base and publishes a sealed physical template with locally packed
  plugin artifacts installed. Container creation copies that sealed tree;
  Container startup must never install or build DSH. `dshbox image` remains a
  deprecated alias forwarding to `build`/`template`. The authoritative design
  is `docs/specs/prepared-template-runtime.md`.
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
- **`src-tauri/dist/` is generated** by Vite before the Tauri build and
  bundled into the desktop binary — never edit files under it.

## Docs to read before touching sensitive areas

| Area | Read |
|------|------|
| Architecture, runtime model, milestones | `docs/HANDOFF.md`, `docs/development.md` |
| Built template design (image pivot) | `docs/specs/image-build.md` |
| Template system behavior | `docs/template-system.md` |
| Plugin pnpm install flow | `docs/design/pnpm-managed-plugin-install.md` |
| Recent bugs / partial fixes | `docs/notes/2026-08-17-bugs-plugin-cache-and-template-not-found.md` |
| Release handoff snapshot | `handoff.md` (repo root) |

## Conventions quick-reference

- Rust: edition 2021, Cargo workspace with `resolver = "2"`. Prefer
  framework-free functions over Tauri-coupled types inside crates.
- Frontend: TypeScript strict, React 18, Vite 6, no extra UI library — build
  primitives under `src/ui/`.
- Persistence: JSON files under the runtime dir + `~/.dsh-box/`. No
  alternative task formats or persistence layouts in adapters.
- Errors: surface them — Box keeps failed diagnostic logs and a recovery view
  rather than silently falling back.
