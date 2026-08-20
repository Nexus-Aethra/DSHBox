# DSH Box

**Managed DeepSeek Harness desktop runtime** — run, isolate, and extend multiple DeepSeek Harness environments on your own machine, no browser tab required.

DSH Box is a lightweight desktop shell built with [Tauri 2](https://tauri.app) that installs, launches, and manages independent DSH **Containers** — each with its own DSH version, profile, plugins, skills, workspace, and logs — and renders them in an embedded WebView.

---
<img width="1920" height="983" alt="image" src="https://github.com/user-attachments/assets/26a17954-b864-43f4-ba19-36f85db738ae" />

## Highlights

- **Isolated DSH Containers** — install multiple DSH versions side by side and create independent Containers per project. Every Container gets its own profile (`web` / `headless` / custom), workspace, plugin set, and host process, so experiments never cross-contaminate.
- **Embedded WebView, no browser needed** — the DSH frontend opens in a native WebView window managed by DSH Box. No port-forwarding, no copy-pasting URLs, no tab clutter.
- **Zero-dependency install** — a private Node, npm, and pnpm runtime is bundled with every release. No system Node, no manual toolchain setup, no PATH hacking.
- **Version manager built in** — browse DSH releases from `deepseek-ai/deepseek-harness`, install or uninstall any tag with one click, and pin a version per Container.
- **Boxfile / built-template pipeline** — describe a Container with a small declarative `.dsh` script (`FROM` + `PROFILE` + `ADD plugin|skill|data`) and `dshbox build` produces a reusable built template; `dshbox run <template>` instantiates and starts a Container. See [Architecture → Boxfile](#boxfile-and-the-built-template-pipeline).
- **In-container agent awareness** — DSH Box injects a `dsh-box-context` plugin into every Container so the in-session agent sees `paths.dshboxHome` and `paths.dshboxCli` and can manage DSH Boxes (containers, templates, plugins) even when it does not inherit a sane `PATH`.
- **Extension & Skill repository** — import plugins and skills from a GitHub URL, a local directory, or a tarball, then install them into any Container's profile with a single click. Skills are auto-sorted into the Container's skill root.
- **Bundle (整合包) workflow** — group any mix of plugins and skills into a named bundle, then export it two ways:
  - **Quick export**: GitHub-sourced entries are kept as URLs, keeping the archive tiny.
  - **Full export**: everything is packed into one portable `.tar.gz`.
  - Bundles can be re-imported (with your choice of *overwrite* or *keep* on name clashes) and installed into any Container — plugins land in the profile, skills are sorted automatically.
- **Smart background tasks** — every long operation (install, start, rebuild, import, export) runs as a visible queued task with real-time scrolling logs, cancel/retry/delete, and history paging. Nothing feels like it "just froze".
- **Dual-mode RPC + live event stream** — the daemon owns all state changes and exposes them through a single `POST /rpc` (sync or async — the daemon decides) and a long-lived `GET /events?token=…` SSE stream. CLI, UI, and external agents talk to the same endpoints; UI pages contain zero business logic.
- **Network-friendly** — automatic proxy detection for GitHub clones, configurable GitHub mirror, and npm registry mirror for installs inside DSH.
- **Background service & tray** — a small `dshboxd` sidecar keeps things tidy, and a system tray icon lets you control it without keeping the main window open.
- **Lightweight by design** — Tauri-based, so the installer is small and the memory footprint stays far below Electron alternatives.
- **Bilingual UI** — English and 简体中文, switchable in Settings.

---

## Install

Download the installer for your platform from the **Releases** page of this repository:

| Platform | Artifact | Notes |
|---|---|---|
| Windows (x64) | `dshbox-<version>-x64-setup.exe` | NSIS installer, per-user and per-machine modes |
| Linux (x64) | `dshbox-<version>-amd64.deb` | Debian/Ubuntu package |
| macOS (arm64) | `dshbox-<version>-arm64.dmg` | Apple Silicon |

> Grab the latest version from the [Releases page](https://github.com/Nexus-Aethra/DSHBox/releases) — artifact names follow the `<product>-<version>-<arch>` convention and may differ per release. Other formats (`.msi`, `.rpm`, `.AppImage`) are produced per release where supported.

No runtime prerequisites — the bundled Node/npm/pnpm runtime travels inside the installer.

---

## Quick start

1. **Launch DSH Box** and pick a writable *runtime directory* when prompted (all DSH data lives there).
2. Open **DSH Version** → **Load versions** → install the DSH tag you want.
3. Open **DSH Container** → create a Container (name, profile, DSH version).
4. Press **Start** — DSH Box builds the frontend if needed (or launches the cached build directly), then opens the DSH UI in the embedded WebView.
5. Head to **Plugin Repository** to import plugins/skills or assemble bundles, then add them to any Container.

### Tray

The app minimizes to the system tray on close. Use the tray menu to open the window or start/stop/restart the `dshboxd` background service.

---

## Architecture

DSH Box separates a Tauri **desktop shell**, a framework-free Rust workspace, a background **daemon** (`dshboxd`), and a small React frontend. The split exists so all business logic — plugin fetching, container lifecycle, template resolution, background tasks — is testable without a UI, and so a CLI or external agent can drive the same flows the UI does.

### Layered components

| Layer | What lives here | Why |
|---|---|---|
| Frontend (React 18 + Vite, `src/`) | Pages, components, `useTaskQueue`/`useContainers`/`useResources`/`useSettings` hooks. **No business logic** — pages fire RPC requests and react to daemon SSE events. | Keeps the Box UI thin and lets any client (UI/CLI/agent) share the same code path. |
| Desktop shell (Tauri 2, `src-tauri/src/`) | Browser window, tray, Tauri IPC adapters. Listens on `127.0.0.1` to the daemon's loopback HTTP server. All real work is delegated to `dshboxd` over HTTP RPC. | One source of truth for state changes — UI and CLI cannot drift. |
| Daemon (`src-tauri/crates/dshboxd`) | Long-lived background service. Owns the queue, the data store, the template index, container registry, and the SSE event bus. Single HTTP entry point (`POST /rpc`) plus `GET /events?token=…`. | Background work (installs, rebuilds, uninstalls) survives the desktop window closing. |
| Crate workspace (`src-tauri/crates/`) | Framework-free Rust crates: `box-foundation`, `box-runtime`, `box-scheduler`, `box-state`, `box-toolchains`, `box-dsh-versions`, `box-containers`, `box-extensions`, `box-image`, `box-template-core`, `box-data-scheduler`, `box-logger`, `box-dsh-context`, `box-server-core`, `box-api`, `box-client`. | Pure functions + unit tests; only the top-level `dshbox` binary and `dshboxd` link Tauri/HTTP. |

The dependency direction is one-way: `foundation / runtime / scheduler / state` → functional crates → Tauri/desktop adapters. Feature crates do not depend on Tauri or one another's mutable state.

### Daemon — dual-mode RPC + SSE event stream

Every UI / CLI action lands on `POST /rpc` with a JSON body of `{"method": "...", "params": {...}, "token": "..."}`. The daemon's dispatch table decides for each handler whether to **synchronously** return JSON (`List templates`, `Read settings`, …) or **asynchronously** enqueue a worker (`Install`, `Build`, `Start container`, `Rebuild`, `Uninstall`, …). Async handlers return a `TaskRecord` immediately; the client subscribes to `GET /events?token=…` for `task:stage` / `task:log` / `task:finished` / `resource:added|updated|removed` events.

This means the same HTTP surface serves every consumer — the desktop app's Tauri IPC handlers, the CLI (`dshbox rpc …`), and external agents calling `curl -d '…' http://127.0.0.1:7923/rpc`. There is no "client fallback" or local-state divergence: the daemon's resource map and task queue are the only sources of truth.

### Boxfile and the built-template pipeline

A **boxfile** (`.dsh`) is the declarative script that describes a Container you want to instantiate. It is parsed by `box-image::parse_script` and resolved by `dshbox build` into a **built template** — metadata-only entries registered in the same content-addressable template store as script templates. `dshbox run <template>` then creates and starts a Container from the template.

The full grammar is in [`docs/template-system.md`](docs/template-system.md); the canonical reference example (every source shape) is in [`examples/boxfile-plugin-chains.dsh`](examples/boxfile-plugin-chains.dsh). Here is the minimal form:

```text
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME my-team

ADD plugin github.com/owner/cordis-plugin-foo:1.2.3
ADD plugin npm:@linxin666/dsh-web-ui-all
ADD plugin ./plugins/secret
ADD skill team-conventions
```

| Directive | Required | Notes |
|---|---|---|
| `FROM <ref>` | yes (exactly once) | GitHub short form (`github.com/owner/repo[:tag|@ref]`), or a local template name (e.g. `web-base`). Up to four levels of template inheritance. |
| `PROFILE <name>` | yes (exactly once) | Target DSH profile (`web`, `headless`, …). |
| `NAME <image-name>` | no | Defaults to the script file's stem. |
| `VERSION <image-version>` | no | Defaults to `latest`. |
| `LABEL key=value` | repeatable | Free-form metadata attached to the built template. |
| `DEF <name> @<path>` | repeatable | Defines a path alias usable as `@<name>` in subsequent `ADD` lines. |
| `ADD plugin\|skill\|data <src> [@<dest>]` | one or more | Resource you want baked into every Container made from this template. |
| `CP <src> [@<dest>]` | alias for `ADD plugin` | Kept for backward compatibility. |

`<src>` accepts four shapes:
1. **GitHub short form** — `github.com/owner/repo[:tag|@ref]`. The GitHub branch resolves through `pnpm pack` and the same fetching/import pipeline as npm; a tagged release becomes a `ref_` on `ParsedSource::Github`.
2. **Tarball** — `https://…/pkg.tgz`, `./relative.tgz`, `/abs/path.tgz`. Anything fetched and unpacked as a tarball.
3. **Local directory** — `./plugins/foo` / `/abs/path/foo`. Imported directly (no archive round-trip) — useful for plugins in progress.
4. **Bare name** — `name[@version]` or `@scope/name[@version]` for plugins already in the Repository.
5. **Explicit prefixes** — `git:…` (clones via libgit2, no guessing) and `npm:…` (registry spec forwarded to pnpm).

The `:latest` tag and the explicit `latest` keyword are interchangeable; both pin the harness repository's main branch.

How each `ADD` is stored matters:
- `ADD plugin` — content stays in the shared **Repository** (one row in `~/.dsh-box/repository/plugins/`); the built template only records a reference id, and reference counts gate deletion.
- `ADD skill` and `ADD data` — snapshotted into the data store (`<runtime>/data/<digest>/`) and copied into the Container profile.
- The bundled `dsh-box-context` plugin (`@deepseek-ai/dsh-box-context`) is linked automatically — you do not need to `ADD` it.

`dshbox build` writes a built template entry keyed by `<name>` (id = fnv1a64 digest). `dshbox run <name>` then:
1. Resolves the template's `FROM` chain (max depth 4),
2. Creates `<runtime>/instances/<id>/{profile,workspace,state,logs}`,
3. Copies plugins/skills/data into the profile,
4. Runs `pnpm install` (with build-scripts approved per-profile),
5. Runs `node --import tsx/esm scripts/build.ts` to build the frontend,
6. Launches the DSH host process and waits on a 20s health probe,
7. Writes `paths.dshboxHome` + `paths.dshboxCli` into the snapshot so the in-container agent can find the CLI.

### Data scheduler and reference counts

Containers, templates, plugins, and skill packs all share a single `resource-map.json` indexed by id. Deletion is `soft-delete → fast queue → permanent delete`; references between Container ↔ template ↔ plugin are kept in lockstep so an entity still in use is never garbage-collected. Full design in [`docs/specs/data-scheduler.md`](docs/specs/data-scheduler.md).

### Logging

`tracing` + `tracing-subscriber` ship structured logs to `<runtime>/logs/<component>.log` (daily rolled) and mirror to stderr. Filter with `RUST_LOG`, e.g. `RUST_LOG=info,dshboxd=debug,box_template_core=debug`.

---

## Technology

| Layer | Stack |
|---|---|
| Desktop shell | Tauri 2, Rust (Cargo workspace under `src-tauri/`) |
| UI | React 18, TypeScript, Vite |
| Background service | `dshboxd` sidecar (single HTTP entry: `POST /rpc` + `GET /events`) |
| Bundled runtime | Node / npm / pnpm (per-platform archive) |
| Targets | Windows x64/arm64, Linux x64/arm64, macOS x64/arm64 |

---

## Building from source

Prerequisites: [Node.js](https://nodejs.org) 20+ with pnpm, and the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform.

```bash
pnpm install
pnpm runtime:prepare    # fetch the bundled Node/pnpm runtime manifest
pnpm server:prepare     # build the dshboxd sidecar
pnpm tauri dev          # run in development
```

Release bundles (per platform):

```bash
pnpm bundle:windows     # Windows NSIS installer
pnpm bundle:linux       # Linux .deb
pnpm bundle:macos       # macOS .dmg
```

Run the test suite:

```bash
cd src-tauri && cargo test --workspace
```

---

## Repository layout

```
src/                       React/TypeScript management UI
src-tauri/                 Rust workspace + Tauri shell
  crates/                  focused, framework-free crates
    box-foundation         config, paths, JSON persistence
    box-runtime            absolute-path process exec
    box-scheduler          persisted task queue + locks
    box-state              ResourceStateManager (read model)
    box-toolchains         bundled Node/pnpm resolver
    box-dsh-versions       DSH GitHub catalogue (harness tag + installs)
    box-containers         Container metadata + active Host registry
    box-extensions         repository plugin/skill scan + transfer
    box-image              .dsh parser, manifest v6, gzip tar I/O
    box-template-core      root/common template install/uninstall core
    box-data-scheduler     soft-delete + dual-queue async hard-delete
    box-logger             tracing init + daily-rolled log files
    box-dsh-context        dsh-box-context plugin (paths.dshboxHome/dshboxCli)
  src/desktop/app/         domain modules (containers, extensions, tasks, …)
examples/                  boxfile.dsh + plugin-chains example
docs/                      HANDOFF.md, architecture.md, template-system.md,
                           specs/, design/, notes/
```

The canonical reference for the boxfile grammar is **`docs/template-system.md`**; image/built-template design lives in **`docs/specs/image-build.md`**; the full RPC + event-stream surface in **`docs/design/rpc-and-events.md`**.

---

## License

Proprietary — see repository owner for licensing terms.

© Nexus-Aethra
