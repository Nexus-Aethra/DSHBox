# DSH Box development guide

## Purpose

DSH Box is a desktop launcher and runtime manager for DeepSeek Harness (DSH). It provides a native window for the DSH Web UI while keeping the development toolchain, DSH versions, profiles, plugins, and skills independently manageable.

The desktop application does not own the DSH React application. It starts a local DSH Web server and renders its loopback URL in a WebView.

The Box management page is its own React application. It is separate from the DSH Web UI and uses a white, minimal visual system: generous spacing, quiet borders, restrained neutral colors, one primary action per view, and concise status text. The Box UI must not imitate or modify DSH's own bundled client.

## Runtime model

## Code modules

The Rust side is a Cargo workspace. Only the `dsh-box` package under `src-tauri/` depends on Tauri. It owns window creation, plugin registration, application startup, and IPC command adapters.

```text
box-foundation  configuration, paths, validation, JSON persistence
box-scheduler   task records, queue state, resource ownership, recovery
box-runtime     absolute-path process execution and libgit2 checkout primitives
box-toolchains  Node, npm, and pnpm discovery and installation
box-dsh-versions DSH repository versions and installation
box-containers  persistent container metadata and host lifecycle
        ↑
dsh-box desktop shell  Tauri state, commands, event forwarding, WebViews
```

Feature crates must not depend on Tauri or one another's mutable state. They use foundation types and small traits from the runtime or scheduler crates. The desktop shell adapts task changes to Tauri events; it does not own a second task format or persistence layout.

```text
Box layer
  ├─ manages Node, npm, pnpm, DSH runtimes, profiles, updates, and logs
  ├─ selects one DSH runtime and its independent profile
  └─ owns the Tauri window and DSH child-process lifecycle
           │
           ▼
DSH layer
  ├─ runs the selected DSH Host and Web server
  ├─ loads only that runtime's profile, plugins, and skills
  ├─ installs the Box bridge plugin on first launch
  └─ renders a Return to Box action in the DSH UI
```

The shell waits for DSH health readiness before opening the main WebView. On shutdown it terminates the child process. A successful plugin install may restart DSH and reload the WebView, because new Node modules are loaded at DSH startup.

## Toolchain management

DSH Box never requires a system Git, Node, npm, or pnpm executable. Runtime source checkout uses libgit2; Node, npm, and pnpm ship as a version-locked private runtime with every application bundle.

The release build prepares the platform runtime from `runtime-lock.json`, verifies Node SHA-256 and pnpm SHA-512 integrity, and bundles the result as an application resource. The resolver invokes npm and pnpm through the bundled Node executable and absolute JavaScript entry points.

```text
tools/
├─ runtime/<platform>/       # read-only application resource
│  ├─ node/
│  ├─ bin/node
│  └─ lib/node_modules/npm/
│  └─ pnpm/node_modules/pnpm/
└─ <user-selected-data-directory>/
```

The process environment exposes only the selected toolchain paths, the selected pnpm store, and the selected DSH home. Diagnostics record the exact executable paths and versions used for every launch or installation.

## User-selected runtime directory

DSH Box stores its small, machine-local control configuration in `~/.dsh-box`. This includes the user-selected runtime directory, the selected DSH version, toolchain selections, window preferences, and update metadata. It must contain no plugin dependency trees, pnpm stores, or runtime archives.

```text
~/.dsh-box/
├─ config.json               # selected runtime directory and defaults
├─ state.json                # last selected DSH version and UI state
├─ toolchains.json           # installed and selected Node/npm/pnpm versions
└─ logs/                     # Box launcher logs
```

The first-run flow asks the user to select a runtime directory. DSH Box manages its large runtime files below that directory:

```text
dsh-box/
├─ runtimes/                 # immutable downloaded DSH versions
├─ current/                  # selected runtime version or pointer
├─ instances/
│  └─ <dsh-version>/
│     ├─ profile/             # package.json, lockfile, cordis.patch.yml
│     ├─ skills/              # user-installed DSH skills
│     ├─ store/               # version-specific pnpm store
│     └─ logs/                # DSH and plugin installation logs
├─ tools/node/               # bundled platform-specific Node and npm runtime
├─ tools/pnpm/               # bundled pinned pnpm runtime
├─ store/                    # pnpm package store
└─ logs/                     # launcher and DSH process logs
```

Every selected DSH version has an independent instance directory. Its package manifest, lockfile, plugin dependencies, skills, pnpm store, and logs do not leak into another DSH version. This makes plugin compatibility and rollback deterministic at the cost of duplicated dependencies.

If the selected runtime directory is unavailable at startup, Box keeps the saved configuration, renders a recovery view, and asks the user to choose a replacement directory. It never silently falls back to a different location.

## DSH instance lifecycle

Selecting a DSH version selects its instance directory and starts its Web server on a loopback port. Before the first successful start, Box installs the `dsh-box-bridge` plugin into that instance profile. The bridge contributes a visible **Return to Box** action to the DSH client.

Selecting **Return to Box** sends a loopback-only bridge request to Box. Box stops the selected DSH child process, returns the WebView to the Box page, and retains the instance logs for inspection. The bridge cannot access arbitrary desktop APIs and cannot navigate the WebView to a non-loopback origin.

Plugin installation runs only in the selected instance profile. The launcher preserves pnpm supply-chain policies and never silently relaxes lifecycle-script approval or minimum-release-age checks.

## DSH update flow

1. Download a versioned DSH runtime artifact into a temporary directory.
2. Verify its checksum and unpack it into `runtimes/<version>/`.
3. Run a local startup health check against the candidate version.
4. Atomically switch `current` to the verified version.
5. Ensure the Box bridge plugin is installed in the new version's instance profile.
6. Restart DSH and restore the WebView URL.

If a health check or startup fails, keep the current runtime selected and retain the failed diagnostic log. Rollback selects an earlier verified runtime without changing the profile.

## Security rules

- Bind DSH only to `127.0.0.1` on a dynamically selected port.
- Pass a per-launch capability token from the shell to DSH and require it for launcher-only endpoints.
- Restrict WebView navigation to the local DSH origin.
- Treat plugin lifecycle scripts as explicit user-approved code execution.
- Verify release checksums before using a downloaded runtime or update.

## Box UI

The React management UI has three primary views:

1. **Home** — selected DSH version, running state, and Start or Return controls.
2. **Versions** — installed DSH runtimes, update availability, switch, rollback, and removal actions.
3. **Toolchains** — installed Node, npm, and pnpm versions, plus the version selected for each DSH runtime.

The UI uses a white background, dark text, muted gray metadata, thin neutral borders, and a single blue primary action. Errors and security approvals remain visible but use color only as a secondary signal. The UI must preserve keyboard navigation, visible focus indicators, and readable contrast.

## Development milestones

1. Build a Tauri Box page for toolchain and DSH version selection.
2. Add managed Node, npm, and pnpm installation with platform-specific version records.
3. Add first-run directory selection and per-DSH-version instance directories.
4. Add a managed DSH child process with readiness detection, shutdown handling, and loopback-only access.
5. Add DSH runtime install, update, restart, health check, and rollback.
6. Implement and auto-install the `dsh-box-bridge` plugin with a Return to Box action.
7. Add per-instance profile, plugin, skill, toolchain, and diagnostics views.

## Validation

Each supported platform must cover the following end-to-end path:

1. Install Node, npm, and pnpm into the selected Box directory without using global tools.
2. Create two DSH versions and confirm their profile, plugin, and skill directories are independent.
3. Start a selected DSH version and load its WebView.
4. Confirm that the Box bridge plugin is installed and Return to Box terminates DSH and restores the Box page.
5. Install a plugin and approve any required lifecycle script, then restart DSH and confirm that plugin loads only in its own instance.
6. Install a DSH skill and confirm it is discovered only by its selected instance.
7. Update the DSH runtime, then roll back to the previous verified version.
