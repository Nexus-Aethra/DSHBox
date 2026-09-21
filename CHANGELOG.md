# Changelog

Notable changes per release. A version tag builds the installers for all three
platforms and publishes them on the
[Releases page](https://github.com/Nexus-Aethra/DSHBox/releases) — see
[Releasing](README.md#releasing).

## 0.1.8

The plugin dependency graph, plus the two packaging faults that kept earlier
builds from ever showing it.

### Added

- **Container resources: extract and inject.** Chat history, provider
  credentials and plugin state are modelled as kinds — location, secrecy, entry
  depth — and can be extracted into the Box store, injected back (refusing by
  default, merging per entry when asked), carried between containers, or packed
  as a tarball. Plugins can declare their own kinds; when they do not, the paths
  their code composes are offered as candidates. `dshbox container resource
  list|stored|extract|inject|rm`, a Resources panel in the container view, and
  `list_container_resources` / `enqueue_resource_*` over RPC. Resource types are
  tabs in the Resources navigation (`Chat history` and `API keys` ship with it;
  **+ Resource type** adds more by detected kind, by scanning a plugin, or by
  picking a path in a tree of the container's storage area), and each tab lists
  every container's copy of that type.
- **The repository stopped duplicating packages.** An import of a package
  (`name`, `npm:name@version`, or a bare name) now records a pointer: the bytes
  stay in pnpm's store and the repository keeps one row, marked `reference` in
  the UI vs `copy` for local directories and archives that pnpm cannot
  recreate. Installing a reference into a container installs its spec (store
  first — measured at one second and zero downloads), and a full bundle export
  materializes it, logging when it had to reach the registry to resolve peer
  ranges.
- **Plugins actually installed are visible, and the repository is the index.**
  The plugin view lists the union of the extension repository and what every
  template and container resolved, read from the profile's `pnpm-lock.yaml`
  (versions included), and the daemon mirrors that back into the repository: one
  derived row per installed plugin, added at startup and refreshed whenever the
  list is read, dropped once nothing installs it, marked 自动收录 and not
  deletable by hand. So a plugin a boxfile installed is simply there — no
  "import into the repository" step, no separate list of what is installed but
  not imported. `plugin_dependency_graph` uses the same lock for its template
  preview, which now shows the plugins a bundle pulled in transitively — with
  pnpm's resolved versions rather than the boxfile's specifier, and with the
  version a `file:` dependency declares rather than its tarball path.
- **Offline readiness is visible.** Each plugin in the list carries the versions
  present in the runtime's own pnpm store (Box pins
  `PNPM_CONFIG_STORE_DIR` under the runtime directory), so it is obvious
  whether installing it needs the network. An unrecognised store layout reports
  "unknown" instead of "not cached", and a `file:`/git dependency is left
  unmarked rather than claimed: pnpm indexes those under the spec, not under
  `name@version`.
- **A document store instead of scattered JSON indexes.** Every persisted
  index now goes through `box_foundation::collection::DocumentStore` (SQLite in
  the new `box-store` crate at `<runtime>/state/dshbox.db`), with legacy files
  migrated on first open and schema changes gated by `PRAGMA user_version`.
- **Plugin dependency graph.** The daemon resolves what a template, a container
  or a running host actually loads — nodes, links, layer depths, load order,
  shared services and parse diagnostics — and the Box UI draws it as labelled
  layer bands with search, click-to-focus and a detail panel. New
  `box-plugin-graph` crate and a `plugin_dependency_graph` RPC; the browser dev
  bridge carries it too.
- **A node per mounted plugin, not per package.** A package that ships both a
  host and a browser half (`dsh.client` + `exports["./client"]`) is drawn as two
  nodes, so `inject`/`provide` resolve inside one context instead of inventing a
  cross-context cycle. A bundle's `cordis.patch.yml` inserts are followed to the
  plugins it mounts.
- **Only real declarations count.** An `inject` is read when a plugin registers
  it — an `Object.assign(target, { inject })` carrier or a factory's returned
  `{ inject, apply }`. On a real profile this removed 26 phantom requirements
  and recovered one genuine one.
- **Templates preview their boxfile plugins.** A template's `ADD plugin`
  entries appear as nodes before any container exists.
- `examples/boxfile-dshell.dsh`: a reference boxfile for a third-party bundle,
  pinned by the parser test suite.

### Added

- **The dependency graph opens folded.** A depth bigger than one band is a depth
  the drawing already has to break into strips, so those are folded on open: a
  175-node container shows as 31 boxes with five of them summaries (`第 3 层 · 28
  个`), and one click on a summary or a band label brings a depth back. A folded
  depth's band label counts the depth itself, not the one box standing in for it.
- **Fold a dependency depth away.** A band's label folds that layer into one
  summary box (`第 3 层 · 28 个`), the layer's edges re-point at it, and the rest
  of the diagram reflows — so a 175-node graph is read one depth at a time. The
  folded depths appear as chips above the canvas, which unfold them again (a
  folded band is gone along with its nodes), and a search that matched inside one
  says how many hits it is hiding.

### Fixed

- **Red-dashed edges meant nothing in particular.** Every edge the layout could
  not draw left-to-right was painted as a cycle — 29 of them on a container with
  no cycle at all. A cycle is a fact about the graph and a crossing is a fact
  about the drawing, and they come apart for one reason: the default view merges
  a package's host and browser halves into one box, so `host A → B` beside
  `browser B → A` draws as a loop that no context has. Cycles are now decided on
  the half-preserving graph and only inside one context (0 on this container, as
  the daemon says), real cycles stay red, and everything else is a quiet grey
  dash with a legend line explaining it. Splitting the halves removes all 29,
  which is what the layering looks like when the contexts are not mixed.
- **The dependency graph cried wolf on a container that runs.** Three findings
  were wrong at once. `profileContext` was reported missing because the launcher
  (`apps/cli`) provides it and the launcher was not a node — it is now scanned
  like a package and drawn as `dsh (launcher)`. `dshell-workspace` was reported
  as waiting on `remote`/`workspaces` whose providers were "installed but not
  active", because a published package that ships `lib/client.js` *and* a
  `lib/client/` directory had its entry file attributed to the **host** half
  (`lib/client.js` does not start with `lib/client`), so the browser half's
  `inject` list landed on the host — the entry file is now a prefix in its own
  right, and the source layout `src/client/index.ts` resolves too. And the
  eight "service names with more than one registration" were all the dual-face
  pattern (one implementation per context), which cordis resolves per isolation
  scope: those are now reported as expected, and only two registrations *in one
  context* stay a conflict. On the real container: 1 missing → 0, 3 inactive
  providers → 0, 8 shared names → 5 expected + 0 conflicts.
- **A slow daemon froze the desktop.** Every Tauri command was synchronous, and
  Tauri runs those on the main thread — the one that renders the window — so a
  single slow daemon call stopped the UI from drawing or responding. The 66
  commands that reach the daemon, the filesystem or another process are now
  `#[tauri::command(async)]` (Tauri's threadpool); only two in-memory reads stay
  synchronous. `box-client` also bounds every RPC with a read timeout and a 3s
  liveness probe, so a daemon that accepts a connection and never answers
  surfaces as "did not answer within 60s" instead of a thread waiting forever,
  and the task poll no longer starts a second request while the previous one is
  still out.
- **A provider key arrived without the provider.** `credentials` was one file
  (`.credentials.yaml`), but DSH keeps a key and the route that uses it in two
  places: the route is `llm-pi-ai.providers.<route>.apiKeyEnv` in
  `settings.yaml`, and `apiKeyEnv` is a credential-ref, so injecting the key
  alone left the target with a secret nothing referenced — the model list stayed
  at one entry. A resource is now one or more *parts*, and a part may be a YAML
  section: `credentials` carries the key file and the `llm-pi-ai` section
  together, merging into `settings.yaml` without touching what other plugins
  keep there. A copy taken before this still injects (its payload has no part
  manifest, so it is the single path it names).
- **The CLI is the agent surface.** `dshbox apply -f <file>` takes one document
  (`container:`, `types:` for the resource types the UI shows as tabs, `copies:`
  for state to extract) and makes the resource layer match it — idempotent, with
  `--dry-run` and `--json` per-entry results, and unknown keys are an error
  rather than a silent no-op. `dshbox container resource types [--json]` lists
  the registered types, `... type rm <view-id>` drops one, and the mutating verbs
  take `--json` too, so an agent can look and act without parsing prose.
- **The CLI and the UI can edit a resource, not just copy it.** `dshbox
  container resource read <id> <path> [--section a.b]` prints a container file's
  YAML as a tree of key paths; `... write <id> <path> --section a.b --text|--file
  [--overwrite] [--restart]` writes one block back. In the Box UI the container's
  storage browser gained **编辑 YAML**: the document is a tree, picking a node
  loads that block, and a path that does not exist yet is how a block is added.
  Both go through the same merge-or-replace policy as an injected copy.
- **A resource copy had no name, and the second one replaced the first.** Every
  copy was keyed `<kind>-<kind>` (`sessions-sessions`,
  `credentials-credentials`), so a type could only ever hold one row and taking
  another copy silently overwrote it — including from a different container.
  Taking a copy is now a button that opens a card asking which container the
  state comes out of and what to call the copy; the name defaults to the source
  container, and taking another copy adds `<name>-2` rather than replacing what
  is there.
- **Creating a container from a template took three minutes.** Every creation
  re-ran the whole client build (native addon, host/client libraries, web
  frontend) — ~180s of the ~195s it took, against 12s of dependency linking.
  That build depends only on the source, the commit and the platform, never on
  the profile's plugins, so it now runs once while the prepared base is
  created and travels into every template built from that base. Container
  creation is a copy plus store links: **195s → 10s** in a measured run. A base
  or template that predates this is repaired or rebuilt in place, and the task
  log says which path it took.
- **Task logs said almost nothing, and said it twice.** A container creation
  logged nothing at all for the ~180 seconds it spent building. Every step now
  logs what it is doing and how long it took, a failed command appends the tail
  of its transcript to the task log, and each line is written once — both
  notifiers were appending to the same file the task context already wrote.
- **Live task progress never reached the UI.** The daemon names its event
  frames `task_stage` / `task_log` / `task_finished` with snake_case payloads,
  while the UI switched on `TaskStage` / `TaskLog` and read a task record out of
  them, so every live update was dropped and the panel fell back to the 3s poll.
- **Installed builds shipped a months-old frontend.** `src-tauri/build.rs`
  mirrored the repo-root `dist/` over `src-tauri/dist` on every build,
  overwriting what Vite had just written, so the desktop binary embedded a
  stale bundle. The mirror is gone; the deb's `dist` resource now points at the
  directory Tauri embeds.
- **The DSH window stayed blank behind a system proxy.** WebKitGTK resolves
  proxies through GIO, whose GNOME resolver sends `127.0.0.1` to the configured
  proxy (its `ignore-hosts` matches a literal `localhost`, not `127.*`). Linux
  now pins `GIO_USE_PROXY_RESOLVER=dummy` before any webview exists.
- **The Linux launcher** now runs `dshbox ui` instead of the bare binary, which
  only printed help.
- **The bundled runtime keeps Node's C headers**, so a container can build DSH's
  `flock` addon on Linux and macOS.
- **Harness version list** is ordered newest first with `latest` on top, and
  no longer sorts `0.1.10` before `0.1.9`.
- **Reading a published package** falls back to `lib/` when it has no sources,
  and looks for a client entry where the manifest says it is.
- **The resource page worked in the browser and failed in the installed app.**
  The resource layer — the type tabs, the copies, the storage browser, the YAML
  editor, the extract and inject verbs, the installed-plugin list — was
  reachable over the daemon RPC and through the browser dev bridge, but the
  desktop shell registered a Tauri command for none of it, so a packaged 0.1.8
  answered every resource call with `Command list_resource_type not found`. All
  thirteen are now `#[tauri::command(async)]` wrappers forwarding to the daemon,
  and a test diffs the command names `box-api.ts` invokes against the
  `generate_handler!` list, so a UI call only the dev bridge can answer fails
  the suite instead of shipping. The installers for this version were rebuilt
  with the fix.

### Infrastructure

- `.github/workflows/release.yml`: a `v*` tag builds Linux (deb + rpm), Windows
  (MSI) and macOS arm64 (dmg) and attaches them to the release; the tag is
  checked against the three version fields first. `workflow_dispatch` builds
  without publishing.
- The Windows bundler finds MSVC through `vswhere` before falling back to
  scanning install roots, and says so when it does fall back: a host whose
  Visual Studio sits in the 64-bit `Program Files` (every GitHub runner) used
  to be handed MinGW silently, which cannot link Tauri's MSVC-flavoured
  dependencies.
- Building needs Node 22.13+: the pinned `packageManager` imports
  `node:sqlite`, which Node 20 does not ship.

## 0.1.7

- Built-template pivot: a sealed physical template carries the profile,
  plugins, skills and data, and containers copy it — startup never installs or
  builds DSH. Design of record: `docs/specs/prepared-template-runtime.md`.
- Bundled Git (Windows) inside the installer, with host-Git passthrough and
  config isolation on Linux.
- Windows compatibility pass: lifecycle fixes for stuck container windows and
  orphaned host processes.

## 0.1.0

First packaged release: Tauri shell, `dshboxd` sidecar, DSH version manager,
containers, extension repository and bundles.
