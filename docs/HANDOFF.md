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

After installing the fixed build, a real Windows machine reported this crash from the bundled Node:

```text
Error: EISDIR: illegal operation on a directory, lstat 'D:'
  at Object.realpathSync ... at Module._findPath ... resolveMainPath
```

`Module._findPath` means a Node process received a drive-relative path (`D:`) as its entry or module request. The leading suspect is a drive-relative runtime directory: if the first-run directory picker stores `D:` instead of `D:\\`, every `PathBuf::join` below it yields drive-relative paths (`D:instances`, `D:runtimes`, ...), which resolve against each child process's current drive and crash inside DSH/pnpm. `save_runtime_directory` in `src-tauri/src/desktop/app/commands/config.rs` currently stores the dialog string as-is and must normalize Windows drive roots (and prefer canonical absolute paths) before persisting. Verification is pending: capture the affected machine's `~/.dsh-box/config.json` `runtimeDirectory` value and the full `desktop.log`.

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

- 2026-08-14: Fixed the Windows startup root cause (platform-aware npm entry in the runtime manifest), removed duplicate root-level `dshboxd.exe` from the installer by moving the sidecar into `crates/dshboxd`, trimmed redundant runtime files, and hardened startup logging. Rebuilt `dshbox_0.1.0_x64-setup.exe`: 33 MB, 2266 files, only `runtime/win-x64` and `server/win-x64` resources. Committed and pushed on `init`. Remaining: the `EISDIR lstat 'D:'` issue above.

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
