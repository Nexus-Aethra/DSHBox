# DSH Box handoff

Updated: 2026-08-14 (Windows packaging fix committed; see Progress log)

## Repository and current state

- Repository: `/home/wpp/homework/dsh-box`
- Current branch: `init`
- The working tree is intentionally dirty. Do not discard or reset it; it contains the latest Container extension, Windows packaging, and diagnostics work.
- The product names are `dshbox` (desktop UI and CLI) and `dshboxd` (background server sidecar). Older names beginning with `dsh-box` still exist in some paths and compatibility code.
- The app is a Tauri v2 desktop shell with a React management UI and a Rust Cargo workspace under `src-tauri/`.

## Product model

DSH Box manages local, independent DeepSeek Harness installations named **Containers**. Each Container has a DSH runtime version, one or more DSH profiles, extensions, a workspace, and logs. The desktop UI controls the state; a DSH Host is started for a Container and rendered in a local WebView.

The user selects a writable runtime directory during first run. The app stores local settings under `~/.dsh-box/`; most runtime data is below the selected directory.

Important current locations below the selected runtime directory:

```text
containers/<container-id>/
  profile/                 # DSH_HOME, profiles, Profile plugin configuration
  workspace/               # Container-scoped working directory for DSH sessions
  extensions/plugins/      # copied Plugin sources from the repository
  logs/                    # host and Container diagnostics
repository/
  plugins/<entry-id>/source/
  skills/<entry-id>/source/
  index.json
state/
  tasks.json
  resources.json
```

The repository is independent from Containers. Importing an extension first stores it in `repository/`; adding it to a Container copies it, so later repository edits and deletion do not mutate installed copies.

## Rust workspace

The workspace crates currently are:

```text
box-foundation    paths, config, JSON persistence, validation
box-runtime       absolute-path process execution and git2/libgit2 checkout
box-scheduler     persisted background task queue, resource locks, cancellation
box-state         ResourceStateManager and diagnostic resource snapshot
box-toolchains    bundled Node/npm/pnpm resolver
box-dsh-versions  DSH GitHub catalogue, runtime installation and removal
box-containers    Container metadata and active Host process registry
box-extensions    repository, Plugin/Skill scan, copy, export and workspace scan
box-server-core   server-oriented composition layer
dshbox            Tauri shell, IPC adapters, CLI and WebView integration
dshboxd           server sidecar binary (own crate: `src-tauri/crates/dshboxd/`)
```

The intended dependency direction is foundation/runtime/scheduler/state -> functional crates -> Tauri desktop adapters. `ResourceStateManager` is the primary read model for lists and details. Long operations should be submitted to `box-scheduler`, not run from a Tauri IPC handler.

## Completed work relevant to current handoff

- Bundled runtime approach: release packages contain a private Node runtime (including npm) and pinned pnpm; DSH Box invokes all three with absolute paths. It does not need system Node, npm, pnpm, or Git.
- DSH runtime checkout uses `git2/libgit2`, rather than a Git executable.
- A global task scheduler and resource state snapshot are present. The UI has task visibility and resource-oriented list/detail reads.
- Container list/detail pages exist. A Container can have profiles, starts a local DSH Host, and exposes Host logs.
- Container context isolation is implemented: Host CWD is the Container `workspace/`; DSH_HOME is its `profile/`; a dshbox-generated system-prompt patch describes the current Container paths to DSH sessions.
- A read-only Plugin/Skill inventory scans Container profile data.
- An independent extension repository now handles Plugin and Skill import, export, deletion, and copied installation into a Container.
- Container workspace scanning/import is newly implemented but not yet committed: it detects Plugins and Skills created under `<container>/workspace`, then lets the UI import them into the independent repository.
- Startup logging is newly implemented: the desktop process logs early startup, resource resolution, bundled-runtime failures, sidecar failures, and panics.

## Current uncommitted changes

The following files are modified or newly created. They are all part of the current state and should be reviewed together before committing:

```text
package.json
scripts/prepare-server.mjs
scripts/bundle-windows.mjs                         (new)
src/App.tsx
src/features/container-details/ContainerDetails.tsx
src/features/plugin-repo/PluginRepo.tsx
src/shared/api/box-api.ts
src/shared/types/domain.ts
src/styles.css
src-tauri/crates/box-extensions/src/lib.rs
src-tauri/crates/box-state/src/lib.rs
src-tauri/crates/dshboxd/Cargo.toml                 (new; replaces src/server/main.rs)
src-tauri/crates/dshboxd/src/main.rs                 (new)
src-tauri/src/desktop/app.rs
src-tauri/src/desktop/mod.rs
src-tauri/src/main.rs
src-tauri/Cargo.toml                                 (dshboxd bin removed, crate added)
src-tauri/tools/runtime-packager/src/main.rs
src-tauri/tauri.conf.json
src-tauri/tauri.linux.conf.json                    (new)
src-tauri/tauri.windows.conf.json                  (new)
src-tauri/resources/runtime/win-x64/               (generated, untracked)
src-tauri/resources/server/win-x64/                (generated, untracked)
```

`src-tauri/gen/schemas/windows-schema.json` is generated during Tauri builds. It should generally not be hand-edited; decide whether generated schema files belong in version control consistently with the existing Linux schema files.

## Windows packaging and startup investigation

### What was done

- The Windows cross-build uses `x86_64-pc-windows-gnu`, MinGW and NSIS.
- `scripts/bundle-windows.mjs` sets the required environment, prepares `win-x64` bundled runtime and `dshboxd.exe`, then invokes Tauri NSIS bundling.
- `scripts/prepare-server.mjs` supports the Windows target and copies `dshboxd.exe` into the sidecar resource directory.
- `src-tauri/tauri.windows.conf.json` explicitly lists only `runtime/win-x64` and `server/win-x64`.
- `src-tauri/tauri.linux.conf.json` explicitly lists only `runtime/linux-x64` and `server/linux-x64`.
- The generic resource mapping was removed from `src-tauri/tauri.conf.json`; platform builds must now use their corresponding config file.
- Desktop startup log path on Windows is `%LOCALAPPDATA%\\dshbox\\logs\\desktop.log`.

### Important unresolved issue

RESOLVED. Two separate problems caused the broken first Windows installer:

1. Resource merging: the original generic resource mapping (`resources/runtime/` and `resources/server/`) was merged with the Windows override instead of being replaced, so both Linux and Windows resources were shipped. The configuration now uses two platform-specific resource maps (`tauri.windows.conf.json`, `tauri.linux.conf.json`) and the generic mapping was removed.
2. Bundled runtime manifest: `runtime-packager` hardcoded the Unix npm entry `node/lib/node_modules/npm/bin/npm-cli.js`. Windows Node archives place npm at `node/node_modules/npm`, so `initialize_bundled_runtime` failed on every Windows start and the app exited during Tauri setup. The manifest entry is now platform-aware.

Additional packaging fixes in the same pass:

- `dshboxd` was moved from a second `[[bin]]` target of the `dshbox` package into its own workspace crate (`crates/dshboxd`). Tauri bundles every binary of the main package next to `dshbox.exe`, which had duplicated `dshboxd.exe` at the installer root; the sidecar now ships only under `server/<target>/`. `scripts/prepare-server.mjs` builds it via its own manifest.
- `runtime-packager` now trims files the app never invokes: corepack and all shims (`npm`, `npx`, `corepack`, `.cmd`, `.ps1`, `install_tools.bat`, `nodevars.bat`), `CHANGELOG.md`/`README.md`, npm `docs/` and `man/`, Node `include/` and `share/`, and the pnpm `artifacts/` directory.
- Release builds set `windows_subsystem = "windows"`; startup now logs OS/arch/PID, the resolved bundled runtime paths, setup completion, and any `tauri::Builder::run()` failure before exiting.

To finish and verify:

```sh
cd /home/wpp/homework/dsh-box
pgrep -af 'makensis|tauri build --bundles nsis' || true
tail -n 30 /tmp/dshbox-windows-final.log

# If no packaging process is running, run a clean new package build.
pnpm bundle:windows

PACKAGE=src-tauri/target/x86_64-pc-windows-gnu/release/bundle/nsis/dshbox_0.1.0_x64-setup.exe
7z l "$PACKAGE" | awk '/runtime\/(linux|win)-x64\/runtime-manifest\.json|server\/(linux|win)-x64\/dshboxd(\.exe)?$/ { print }'
```

The final command must show only `runtime/win-x64/...` and `server/win-x64/dshboxd.exe`; there must be no `dshboxd.exe` at the installer root.

### Why the old installer appeared to start with no logs

The app did start and wrote `%LOCALAPPDATA%\\dshbox\\logs\\desktop.log`, but Tauri setup failed immediately because the bundled runtime check could not find the npm entry. Users checking the legacy `~/.dsh-box/logs` location found nothing. Keep the desktop log path `%LOCALAPPDATA%\\dshbox\\logs\\desktop.log` as the authoritative Windows diagnostic.

### Open issue: Node `EISDIR lstat 'D:'` on a real Windows machine

RESOLVED in code (native Windows build pending user verification). The crash came from a drive-relative runtime directory: the first-run picker could persist `D:` (drive root) verbatim, and every `PathBuf::join` below it yielded `D:containers`-style paths that resolve against each child process's current drive, crashing bundled Node with `EISDIR lstat 'D:'` inside `Module._findPath`.

Fix, all in the current worktree:

- `box-foundation` gained `normalize_runtime_directory`: canonicalizes via `fs::canonicalize`, treats a bare Windows drive root (`D:`) as rooted (`D:\`), resolves relative entries against the current directory, and strips the verbatim `\\?\` prefix `canonicalize` adds on Windows.
- `save_runtime_directory` (`src-tauri/src/desktop/app/commands/config.rs`) persists the normalized value instead of the raw dialog string.
- `read_config` defensively re-normalizes any legacy `runtimeDirectory` value on load, healing configs already written with `D:`.
- Three unit tests cover drive-root normalization, canonicalization, and rejection of nonexistent directories.

Second root cause found on the real machine (same EISDIR stack): Tauri's `resource_dir()` returns verbatim `\\?\` prefixed paths on Windows, and bundled Node crashes with `EISDIR lstat 'D:'` when a verbatim entry script (`\\?\D:\...\pnpm.cjs`) reaches `Module._findPath`. Fixed in `initialize_bundled_runtime` and `bundled_server_path` by stripping the prefix via the new public `box_foundation::strip_verbatim_prefix`.

Third fix: pnpm 11 auto-downloads the project's pinned `packageManager` version when it differs from the bundled pnpm (DSH pins `pnpm@11.7.0`), stalling startup. Resolved by aligning the bundled pnpm to the pinned version: `runtime-lock.json` now pins pnpm 11.7.0 (matching deepseek-harness's `packageManager`), so no switch/download occurs.

Fourth fix: pnpm's own `runDepsStatusCheck` re-spawns a bare `pnpm` command (or reuses `node <pnpm.mjs>` when the ESM entry is used); DSH build scripts also call bare `npm`/`pnpm`. The bundled runtime previously shipped no command shims and was not on PATH, so Windows failed with "'pnpm' is not recognized". The runtime now records `pnpm.mjs` as the pnpm entry (when present) so pnpm re-spawns itself through the bundled node, and `runtime-packager` writes `pnpm.cmd`/`npm.cmd` (Windows) and `pnpm` (Unix) shims next to the runtime; `command_for_toolchain` prepends those directories to PATH for every spawned tool. `start_dsh_container_with_task` also now runs `pnpm install` before the first frontend build when the fresh checkout has no `node_modules`.

Fifth fix: every spawned console child (node, pnpm, npm, schtasks) popped a black terminal window because the GUI process has no console and `CREATE_NO_WINDOW` was not set. `box_foundation::suppress_console_window` now sets the flag on all `Command` spawn sites: `command_for_toolchain` and the npm version probe in `app.rs`, `cli.rs` plugin add, `box-runtime::NativeProcessRunner`, `box-toolchains::command_version`, and the schtasks calls in `box-server-core`.

Sixth fix: the PATH prepend in `command_for_toolchain` derived the pnpm bin dir from `runtime.pnpm.parent()`, but after switching the pnpm entry to `pnpm.mjs` that parent is `pnpm/node_modules/pnpm/bin` (no `pnpm.cmd`), so bare `pnpm` still failed in DSH build scripts while bare `npm` worked. The bin dirs are now derived from the node executable's parent (`runtime/<target>/node` and sibling `runtime/<target>/pnpm`). Verified end-to-end: `pnpm run build` (including its `npm run build:lib && npm run build:web` nesting) succeeds with the bundled runtime on PATH.

Seventh fix: long installs/builds looked stuck because pnpm output only went to the log file, not the task log the UI shows. `spawn_forwarding_log` now spawns with piped stdout/stderr and forwards every line to both the log file and the task's live log (`task://log`), for the first-run install, the DSH build, and the rebuild flow.

Eighth fix: "open container" produced a black, unclosable window with `dshbox.exe` hanging (Windows Application Hang 1002). `open_dsh_front` now dispatches `WebviewWindowBuilder::build()` through `run_on_main_thread`, adds `--disable-gpu-compositing`, and builds the window with `WebviewUrl::App` then jumps to the DSH host URL via `window.eval("location.href=...")` because wry 0.55 on Windows silently skips the initial navigation of `WebviewUrl::External` windows (diagnosed live: zero TCP connections from WebView2 to the host, zero renderer CPU, while the same URL renders perfectly in a normal browser). A 15-second window-title probe falls back to the system browser via `webbrowser::open`, and the container details Logs tab has a manual "Open in browser" button (`open_dsh_front_browser`).

Ninth fix (Windows WebView2 black window): superseded by the eval-based approach in the Eighth fix entry above — the intermediate attempt used an explicit `window.navigate(url)` after build, which still failed on this machine. The 15-second window-title probe and browser fallback described there remain valid.

Tenth fix (critical): after the dev-deploy shortcut (`scripts/dev-deploy.ps1` runs plain `cargo build` instead of `tauri build`), the app showed `ERR_CONNECTION_REFUSED` on its own main window. Root cause: tauri's `is_dev()` is `!cfg!(feature = "custom-protocol")`; the tauri CLI passes `--features custom-protocol` during `tauri build`, but plain `cargo build` does not, so the binary ran in dev mode and navigated the main window to `devUrl` (`http://localhost:1420`, no vite server running) instead of serving the embedded frontend. Fix: added `custom-protocol = ["tauri/custom-protocol"]` to `src-tauri/Cargo.toml` features and `--features custom-protocol` to `dev-deploy.ps1`. Any future manual release build of the desktop binary must include this feature.

Tenth feature: mirror settings in Settings > General — `githubMirror` (free-form prefix applied to GitHub tags API, runtime clones, extension imports) and `npmRegistry` (preset dropdown: npmjs/npmmirror/Tencent/Huawei + custom, applied to spawned pnpm/npm via `npm_config_registry`). Saved through `save_mirror_settings` into `BoxConfig`.

Verification is pending: install the newly built native Windows installer, set the runtime directory, and confirm `~/.dsh-box/config.json` stores an absolute path while DSH/pnpm sessions start cleanly.

### Windows service limitation

`dshboxd` currently does **not** implement the Windows named-pipe transport. The Windows branch in `src-tauri/crates/dshboxd/src/main.rs` prints:

```text
dshboxd named-pipe transport is not implemented for this target
```

and exits. This means Windows background server/service support is incomplete. It should not be treated as a solved feature. The desktop UI should still be made able to open independently, but system-service functionality requires a real named-pipe server, client transport and Task Scheduler integration.

### Diagnosing a Windows launch failure

1. Install the newly verified NSIS package.
2. Start `dshbox` once.
3. Read `%LOCALAPPDATA%\\dshbox\\logs\\desktop.log`.
4. Collect Windows Event Viewer application errors if the process fails before Rust can write a log.
5. Check Microsoft Edge WebView2 Runtime. Tauri's Windows UI needs WebView2; an absent or broken runtime can prevent a window from appearing.
6. The current cross-built installer is unsigned, so SmartScreen/Defender can also block it. That is separate from a program crash.

Do not guess the cause from the prior report alone; use `desktop.log` from the new build. The current log hook catches Rust panics and setup errors, but native loader failures may appear only in Event Viewer.

## Build and validation commands

Frontend build:

```sh
pnpm run build
```

Rust validation:

```sh
cd src-tauri
cargo check --workspace
cargo test --workspace
```

Linux package:

```sh
pnpm bundle:linux
```

Windows package from this Linux host (requires the Rust target, MinGW and NSIS):

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install -y gcc-mingw-w64-x86-64 nsis p7zip-full
pnpm bundle:windows
```

The current session successfully ran `pnpm run build`, `cargo check --workspace`, and a Windows cross-target check before the final packaging configuration change. Re-run the appropriate checks after resolving the still-running final Windows bundle.

## Progress log

- 2026-08-14: Fixed the drive-relative runtime directory root cause behind `EISDIR lstat 'D:'`. Added `normalize_runtime_directory` to `box-foundation` (canonicalize + drive-root handling + verbatim-prefix stripping), applied it in `save_runtime_directory` and defensively in `read_config`, with 3 unit tests. Also fixed local Windows dev bootstrap: `pnpm-workspace.yaml` lacked the required `packages` field, so pnpm failed with "packages field missing or empty"; added `packages: []` and used the project-pinned pnpm 11.21.0. Built a native Windows MSVC NSIS installer for manual verification.
- 2026-08-14: Second EISDIR root cause on the real machine: Tauri `resource_dir()` returns `\\?\`-prefixed paths and bundled Node crashes on verbatim entry scripts. Stripped the prefix for the bundled runtime and sidecar paths (`strip_verbatim_prefix` in `box-foundation`, applied in `initialize_bundled_runtime`/`bundled_server_path`).
- 2026-08-14: Aligned bundled pnpm to the DSH project's pinned version (11.7.0) in `runtime-lock.json` so pnpm 11's automatic `packageManager` version download/switch never fires (it stalled startup). Added a first-run `pnpm install` before the DSH frontend build.
- 2026-08-14: Fixed bare `pnpm`/`npm` resolution for bundled-tool spawned processes: runtime manifest now uses the `pnpm.mjs` ESM entry (pnpm re-spawns itself through bundled node instead of a PATH lookup), `runtime-packager` writes `pnpm.cmd`/`npm.cmd` (Windows) and `pnpm` (Unix) command shims, and `command_for_toolchain` prepends the bundled bin directories to PATH. This unblocks pnpm's dependency-status check and DSH build scripts (`npm run build:lib`, `pnpm --filter ...`).
- 2026-08-14: Suppressed the black console windows that every spawned child (node/pnpm/npm/schtasks) opened on Windows: added `box_foundation::suppress_console_window` (`CREATE_NO_WINDOW`) and applied it to all `Command` spawn sites across `app.rs`, `cli.rs`, `box-runtime`, `box-toolchains`, and `box-server-core`.
- 2026-08-14: Fixed the PATH-prepend bin-dir bug in `command_for_toolchain` (pnpm dir was derived from the `pnpm.mjs` entry's parent, which has no `pnpm.cmd`); bin dirs now come from the node executable's sibling layout. Verified `pnpm run build` end-to-end with the bundled runtime. Also added `spawn_forwarding_log` so install/build output streams live to the task log the UI renders (progress instead of looking stuck).
- 2026-08-14: Fixed the black unclosable DSH window hang (Application Hang 1002): `open_dsh_front` now builds the WebView window on the main thread via `run_on_main_thread` (IPC-thread window creation blocked the message pump) and adds `--disable-gpu-compositing` for WebView2 black screens; open failures are logged to `desktop.log`.
- 2026-08-14: Root-caused the persistent black DSH window: wry 0.55 on Windows silently skips the initial navigation of `WebviewUrl::External` windows (WebView idle, no TCP connections to the host). `open_dsh_front` now re-navigates explicitly via `window.navigate()` and falls back to the system browser after a 15s window-title probe; added `open_dsh_front_browser` command + "Open in browser" button on the container Logs tab (webbrowser crate).
- 2026-08-14: Added mirror settings (Settings > General): `githubMirror` free-form prefix for GitHub tags/clones/imports and `npmRegistry` preset dropdown (npmjs/npmmirror/Tencent/Huawei/custom) applied via `npm_config_registry` to spawned pnpm/npm.

## Recommended next work

1. ~~Finish the Windows NSIS build and verify it contains no Linux resources.~~ Done; additionally verify the npm entry fix on a real Windows machine.
2. Test the installer on a real Windows machine and capture `%LOCALAPPDATA%\\dshbox\\logs\\desktop.log` if it fails.
3. Implement Windows named-pipe server transport and current-user Task Scheduler lifecycle before claiming `dshboxd` Windows support.
4. Verify the generic configuration removal does not affect Linux development/resource discovery; package a Linux `.deb` with `pnpm bundle:linux` and smoke-test it.
5. Add tests around extension repository refresh after import/copy/delete, Container extension removal, and workspace scanning/import.
6. Review the existing long `docs/development.md`; parts of its directory diagram and UI description predate Containers, the independent repository and the dshbox/dshboxd split.
7. Split the current mixed worktree into logical commits only after validation: extension repository/workspace work; startup diagnostics; Windows packaging; docs.

## Collaboration notes

- Avoid `git reset --hard`, `git checkout --`, or broad cleanup commands: there are substantial local changes.
- Generated runtime folders are build products and should remain ignored unless the release process intentionally vendors them in source control.
- All user-visible long operations should remain scheduler tasks. Do not add blocking network, clone, pnpm build, Host-ready polling, or broad scan work directly to desktop IPC handlers.
- The most reliable current sources of truth are `ResourceStateManager` and the runtime state snapshots, not per-page React state or direct filesystem scans from the UI.
