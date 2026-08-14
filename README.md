# DSH Box

**Managed DeepSeek Harness desktop runtime** — run, isolate, and extend multiple DeepSeek Harness environments on your own machine, no browser tab required.

DSH Box is a lightweight desktop shell built with [Tauri 2](https://tauri.app) that installs, launches, and manages independent DSH **Containers** — each with its own DSH version, profile, plugins, skills, workspace, and logs — and renders them in an embedded WebView.

---

## Highlights

- **Isolated DSH Containers** — install multiple DSH versions side by side and create independent Containers per project. Every Container gets its own profile (`web` / `headless` / custom), workspace, plugin set, and host process, so experiments never cross-contaminate.
- **Embedded WebView, no browser needed** — the DSH frontend opens in a native WebView window managed by DSH Box. No port-forwarding, no copy-pasting URLs, no tab clutter.
- **Zero-dependency install** — a private Node, npm, and pnpm runtime is bundled with every release. No system Node, no manual toolchain setup, no PATH hacking.
- **Version manager built in** — browse DSH releases from `deepseek-ai/deepseek-harness`, install or uninstall any tag with one click, and pin a version per Container.
- **Extension & Skill repository** — import plugins and skills from a GitHub URL, a local directory, or a tarball, then install them into any Container's profile with a single click. Skills are auto-sorted into the Container's skill root.
- **Bundle (整合包) workflow** — group any mix of plugins and skills into a named bundle, then export it two ways:
  - **Quick export**: GitHub-sourced entries are kept as URLs, keeping the archive tiny.
  - **Full export**: everything is packed into one portable `.tar.gz`.
  - Bundles can be re-imported (with your choice of *overwrite* or *keep* on name clashes) and installed into any Container — plugins land in the profile, skills are sorted automatically.
- **Smart background tasks** — every long operation (install, start, rebuild, import, export) runs as a visible queued task with real-time scrolling logs, cancel/retry/delete, and history paging. Nothing feels like it "just froze".
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

## Technology

| Layer | Stack |
|---|---|
| Desktop shell | Tauri 2, Rust (Cargo workspace under `src-tauri/`) |
| UI | React 18, TypeScript, Vite |
| Background service | `dshboxd` sidecar |
| Bundled runtime | Node / npm / pnpm (per-platform archive) |
| Targets | Windows x64/arm64, Linux x64/arm64, macOS x64/arm64 |

The Rust codebase is organized as a workspace of focused crates (`box-foundation`, `box-scheduler`, `box-runtime`, `box-toolchains`, `box-dsh-versions`, `box-containers`, `box-extensions`, `box-state`, `box-server-core`) with a thin desktop adapter layer — task scheduling, extension transfer, and container lifecycle logic are framework-free and unit-tested.

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
  src/desktop/app/         domain modules (containers, extensions, tasks, …)
docs/HANDOFF.md            architecture & operation notes
```

---

## License

Proprietary — see repository owner for licensing terms.

© Nexus-Aethra
