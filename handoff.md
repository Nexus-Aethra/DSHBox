# DSH Box — Handoff Document

> Status snapshot for project handover (2026-08-17). Test count is now
> **107 passing** (1 pre-existing `import_dedup_by_name_and_version` flake —
> see *Known issues* below). The build pipeline spec lives in
> `docs/specs/image-build.md`. The construct is a TEMPLATE only — `dshbox
> build` produces a metadata-only *built template* in the same store as
> pulled script templates; there is no separate image registry (`dshbox
> image` survives as a deprecated alias).

## Current State Summary

**Verification**: cargo test --workspace 107 pass / 1 flake | pnpm tsc --noEmit passes | scripts/e2e-*.sh (sandbox-isolated) all pass

**Branch**: `feat/resource-management` (HEAD = `2d75261`)

**Recent commits (oldest → newest)**:

| Commit | What |
|---|---|
| `0f86c2e` | feat(image):落地镜像构筑架构——build 产镜像、run 从镜像建容器 (architecture pivot: image → built template) |
| `f263ae4` | fix(build): plugin cache hit (skip duplicate `img-<id>` rows on second import) + `run` start "template not found" (legacy flat-file path) |
| `9d0847d` | fix(lookup): `template_content_path` was hardcoded to `script.dsh` — built templates (`list.json`) now resolve via `lookup_template_path` |
| `66524c2` | test(e2e): 8-step regression covering build → run built template |
| `f92536e` | fix(extension): link `node_modules` into container plugin source — **partial fix, see Known issues** |
| `2d75261` | feat(container): `describe` / `show` / `open` / `rm` actions on `dshbox container` |

Uncommitted working tree (NOT yet reviewed): `docs/specs/image-build.md`,
`docs/template-system.md`, `examples/boxfile.dsh`, `handoff.md`,
`src-tauri/crates/{box-dsh-versions,box-image}/`, `src-tauri/src/cli/{build,image,init,run,template}.rs`,
`src-tauri/crates/dshboxd/src/containers.rs`, `src/shared/{types,state}/`.
These belong to the in-progress built-template consolidation (the 0f86c2e
branch); review and commit together before opening a release branch.

---

## Architecture

### Crates (src-tauri/crates/)

| Crate | Role |
|-------|------|
| box-image | .dsh script parser + .dshimage manifest v6 + gzip tar archive I/O |
| box-state | ResourceStateManager — in-memory snapshot of all resources |
| box-extensions | Repository/plugin/skill scanning, ExtensionKind (Plugin|Skill) |
| box-foundation | Config, paths, utilities |
| box-containers | Container scanning and creation |
| box-dsh-versions | DSH version catalog and installation |
| box-scheduler | Task queue, TaskManager, TaskClient |
| box-runtime | shallow_clone_with_cancel for GitHub clones |
| box-toolchains | Node/pnpm toolchain resolution |
| box-server-core | Background service (dshboxd) helpers |
| box-dsh-context | DSH patch YAML / context snapshot rendering |

### Main binary (src-tauri/src/desktop/)

| Module | Key Functions |
|--------|--------------|
| app.rs | Tauri entry, ContainerManager, graceful shutdown |
| app/image.rs | build_image_from_script, preview_image_script, validate_archive |
| app/bundles.rs | install_container_plugin, install_container_skill, bundle CRUD |
| app/extensions.rs | import/export/copy repository extensions |
| app/containers.rs | create_dsh_container_sync, container lifecycle |
| app/lifecycle.rs | start_dsh_container_with_task, process tree management |
| app/versions.rs | DSH version install/uninstall |
| app/toolchains.rs | resolve_toolchain, command_for_toolchain |
| app/services.rs | initialize_bundled_runtime |

### CLI (src-tauri/src/cli/)

| Command | File | Status |
|---------|------|--------|
| dshbox info/ps | mod.rs | Done |
| dshbox rpc | mod.rs | Done (raw RPC debug escape hatch) |
| dshbox pull template | pull.rs | Done (libgit2 clone + hash-indexed template store) |
| dshbox init | init.rs | Done (starter boxfile) |
| dshbox build | build.rs | Done (produces a metadata-only built template) |
| dshbox run | run.rs | Done (built template first, script template materialises live) |
| dshbox container | container.rs | Done (logs/url/start/stop/rebuild) |
| dshbox template | template.rs | Done (ls/show/import/export/rm/prune, both forms) |
| dshbox plugin | plugin.rs | Done |
| dshbox bundle | bundle.rs | Done |
| dshbox image | image.rs | Deprecated alias forwarding to build/template |
| dshbox config | config.rs | Done |

Removed: `dshbox dsh` (subcommand dropped), `dshbox resources` (superseded by plugin/bundle).

> Build model: `dshbox build` produces a metadata-only built template
> (plugins referenced from the repository, every other kind hashed into the
> data store as snapshots) registered in the same content-addressable
> template store as pulled script templates; containers are created from
> either template form via `dshbox run`. Full design:
> `docs/specs/image-build.md`.

### Frontend (src/)

| File | Role |
|------|------|
| App.tsx | Shell: navigation (Container|Resources|Settings), state wiring |
| i18n.ts | All UI strings (EN + zh-CN) |
| shared/types/domain.ts | All TypeScript types |
| shared/api/box-api.ts | Tauri IPC bridge |
| features/resources-page/ResourcesPage.tsx | NEW — 4 tabs (Harness|Plugin|Bundle|Template) |
| features/container-details/ContainerDetails.tsx | Container detail view |
| features/tasks/TaskPanel.tsx | Task list panel with logs |
| features/toolchains/ToolchainRow.tsx | Toolchain status display |
| state/useResources.ts | NEW — unified hook (plugins, bundles, template build) |
| state/useContainers.ts | Container list, create, start/stop, details |
| state/useSettings.ts | Config, DSH versions, toolchains |
| state/useTasks.ts | Task polling, log display |

### Removed Files
- src/features/plugin-repo/PluginRepo.tsx — replaced by ResourcesPage
- src/state/useRepository.ts — replaced by useResources
- src/state/useImages.ts — replaced by useResources
- src-tauri/src/cli/dsh.rs — orphaned leftover of the removed `dsh` subcommand (untracked; safe to delete)

---

## box-image: Manifest v6

### Script syntax (.dsh)

FROM <harness-source>          # required
PROFILE <name>                 # required
NAME <name>                    # optional
VERSION <version>              # optional
LABEL key=value                # optional, repeatable
DEF <name> @<path>            # optional, repeatable (path alias)
ADD plugin|skill|data <src> [@<dest>]   # one or more


### Source shapes

- GitHub: github.com/owner/repo[:tag|@ref]
- Tarball: https://... or ./local.tgz
- Local dir: ./path or /abs/path
- Bare name: name[@version] or @scope/name[@version]
- Container path: [@]<container-id>@/path (future)


### DEF built-in defaults

| Name | Path | Meaning |
|------|------|---------|
| plugin | @profile/profiles/<profile>/node_modules | Plugin install dir |
| skill | @profile/skills | Skill install dir (= $DSH_HOME/skills) |
| data | (empty) | Must be explicit |


### Manifest structure (schema version 6)

mediaType: "application/vnd.dshbox.template.v6+json"

ImageManifest {
    schema_version: 6
    base: TemplateBase::Runtime { ref_ } | TemplateBase::Template { id }
    profile: String
    defs: BTreeMap<String, String>      // resolved DEF table
    adds: Vec<ResolvedAdd>              // ordered cp operations
    labels: BTreeMap<String, String>
    include_data: bool
}

ResolvedAdd {
    kind: AddKind          // Plugin | Skill | Data
    source: AddSource      // Github | Tarball | LocalPath | PluginPath | ContainerPath
    destination: String    // container-relative path
    blob: String           // archive blob path
    digest: String         // fnv1a64 hex
}


### Key types

- AddKind (in script.rs): Plugin | Skill | Data — with to_extension_kind() converter
- AddSource (in manifest.rs): Github | Tarball | LocalPath | PluginPath | ContainerPath
- TemplateBase (in manifest.rs): Runtime { ref_ } | Template { id }

---

## Install Paths (verified against harness source)

### Plugin

- Source stored: container/extensions/plugins/<id>/source/
- Linked via: pnpm dsh plugin --profile <name> add <path>
- DEF default: @profile/profiles/<profile>/node_modules
- Resolves to: profile/profiles/<profile>/node_modules/<name>

### Skill

Harness lookup (skill-filesystem/src/index.ts lines 246-253):

1. <cwd>/.dsh/skills/ (project-local)
2. customSkillDirs (user-configured)
3. $DSH_HOME/skills/ (user-global)

Container DSH_HOME: container/profile/ (set at startup via .env("DSH_HOME", ...))

Install path: container/profile/skills/<name>
DEF default: @profile/skills -> profile/skills  (verified correct)


## UI Navigation

Top bar:  [Container] [Resources] [Settings] [Tasks]

### Container page

- List containers (start/stop/open/delete/rebuild)
- Create container form (name, profile, DSH version)
- Container detail view (profiles, plugins, skills, logs, workspace extensions)

### Resources page (4 tabs)

| Tab | Content |
|-----|---------|
| Harness | DSH version list, install/uninstall, refresh catalog |
| Plugin | Repository plugin list, import (GitHub/tarball/local), export, delete |
| Bundle | Bundle list, create from plugins, import/export/delete |
| Template | Choose .dsh script -> preview -> build .dshimage |

### Settings page

- Storage directory, background service, GitHub mirror, npm registry, language


## ResourceKind enum (box-state)

System-level: Toolchain, Runtime, Container, Task
User-facing:  Harness, Template, Plugin, Bundle


## Known Issues / Future Work

Resolved since this document was first written: dshboxd compiles and is the
production daemon; template listing/import/export works (hash-indexed store);
ADD data is implemented (content-addressed store under `<root>/data/`);
base templates are generated on harness pull; template FROM chaining works
(depth limit 4); Windows named pipes are implemented; `container describe`/
`show`/`open`/`rm` actions exist (commit `2d75261`); plugin cache hit dedup
on import (commit `f263ae4`); built templates resolve through the hash
index (commit `9d0847d`).

### Open bug: boxfile `ADD plugin` still externalises transitive deps at build time

**Symptom** (user runtime, container-1786943199, `dsh-better-sidebar@0.11.0`):

```
[UNRESOLVED_IMPORT] Could not resolve '@univerjs/presets' in ...xlsx-to-univer.ts
                   Module not found, treating it as an external dependency
[UNRESOLVED_IMPORT] Could not resolve 'xterm'           in ...TerminalView.tsx
[UNRESOLVED_IMPORT] Could not resolve '@xterm/addon-fit' in ...TerminalView.tsx
```

Bundled `lib/client.js` ends up with `require("clsx")` left in; DSH runtime's
client loader then errors with `clsx missed the module table`.

**Root cause** (verified against the user's runtime 2026-08-17):

1. `import_into_repository` (`src-tauri/crates/dshboxd/src/extensions.rs`)
   copies the plugin source into `<root>/repository/plugins/<img-id>/source/`
   but **never runs `pnpm install`**. So repo entries have NO `node_modules/`.
2. `install_plugin_to_container_mode` (`box-extensions/src/transfer.rs`)
   walks the repo entry and creates the container's source as a series of
   **symlinks**: `src -> <repo>/source/src`, `node_modules -> <repo>/source/node_modules`
   (the latter added by `f92536e`). But the link target is empty, so the
   container side ends up with an empty `node_modules` link.
3. `tsdown`/`rolldown` resolves imports from the symlinked source files using
   **real paths** (`<repo>/source/src/client/x.tsx`) and walks UP looking for
   `node_modules`. The first `node_modules` it finds is at
   `<repo>/source/node_modules` (empty link target), then `<repo>/node_modules`
   (does not exist), then `<container>/source/node_modules` — but Node uses
   real paths, so it never reaches the container's side.
4. Every transitive dep (`clsx`, `@univerjs/presets`, `xterm`, …) gets
   externalised. The build "succeeds" but the bundle is broken at runtime.

**Workaround in production**: `dsh plugin --profile web add dsh-better-sidebar@latest`
(the official DSH CLI install path, mapped from `dshbox plugin install`). It
runs `pnpm add` directly into the profile directory, so the pnpm `.pnpm/`
layout is real and `tsdown` (the plugin's `prepare` script) resolves deps
through the standard Node module resolution. Verified working in the user's
runtime.

**Proper fix direction (do not lose this when resuming)**:

* `import_into_repository` must run `pnpm install` in the repo entry after
  the copy. A partial implementation is in the working tree (`extensions.rs`,
  guarded by `plugin_declares_deps` so empty-fixture tests still pass), but
  it is **uncommitted**. Needs review, the env_lock env dance for the
  bundled runtime, and a real e2e round-trip.
* `preflight_profile_plugins` should rebuild `lib/` when the source mtime
  is newer than `lib/`'s (today it skips when `lib/index.js` exists, which
  locks in a broken bundle forever).
* The `f92536e` "link `node_modules`" change is correct in isolation but
  is a no-op until step 1 is in place — the link target has to exist.

**User-visible impact today**: anyone running `dshbox run` on a built
template whose plugin came through the boxfile `ADD plugin` path will hit
the externalised-bundle error. The boxfile `ADD plugin` path is the primary
authoring flow, so this needs fixing before the next release. The CLI
workaround is documented above.

### Test flake: `import_dedup_by_name_and_version`

After adding pnpm-install-to-import (working tree, uncommitted), the existing
dedup test in `extensions.rs` fails with `bundled runtime is unavailable;
start dshboxd first` because the new code path calls `resolve_toolchain("pnpm")`,
which requires the bundled runtime to be initialised. The `plugin_declares_deps`
guard short-circuits empty-fixture plugins, so a fix on the test side (call
`initialize_bundled_runtime()` in the test's `setup`) is the right call once
the uncommitted code is reviewed.

### UI built-template surface

Built templates now share the template index (the separate image registry was
removed); the desktop UI lists them through the template list but has no
dedicated built badge / form column yet (containers are still created from
templates in the dialog); see `docs/specs/image-build.md` section 8.

### Container-path sources

`[@]<container-id>@/path` parser support is partial.

### Batch pnpm hook

`pnpm dsh plugin add` is called per-plugin; could batch into one call.

### Runtime daemon rebuild for the user

The user's installed `/usr/lib/dshbox/server/linux-x64/dshboxd` is an older
binary (predates the boxfile fixes). After the uncommitted fixes are
finalised and tested, re-bundle the deb/rpm (`pnpm bundle:linux`) and reinstall
so the user's runtime daemon picks up `describe_container`, the boxfile
fixes, and the new import-time install path.

### `dshbox run --name` (verified)

Already supported in `src-tauri/src/cli/run.rs` line 23
(`flag_value(arguments, "--name", &template)`). The default is the template
name; pass `--name <container>` to override. Documented in the Quick Start
section of the global help (`dshbox help`). No code change needed.

---

## Run Commands

```sh
# Backend
cd src-tauri
cargo test --workspace
cargo build --release -p dshboxd -p dshbox

# Frontend
pnpm tsc --noEmit

# E2E (isolated daemons, safe to run)
bash scripts/e2e-pull-list.sh
bash scripts/e2e-container-skill.sh
bash scripts/e2e-catalog.sh
bash scripts/e2e-buildrun-from-built-template.sh

# Dev server (requires Tauri)
cd src-tauri && cargo tauri dev

# Re-bundle Linux package (after fixing the open bug above)
pnpm bundle:linux
sudo dpkg -i dist/dsh-box_*.deb   # or: sudo rpm -Uvh dist/dsh-box-*.rpm
systemctl --user restart dshboxd
```

---

## Resume checklist (in priority order)

1. Review and commit the uncommitted `import_into_repository` pnpm-install
   change (working tree). Run the dedup test under `initialize_bundled_runtime`
   to clear the flake.
2. Extend `preflight_profile_plugins` to rebuild `lib/` when the source mtime
   is newer than `lib/`'s, so existing broken bundles self-heal on next start.
3. Add e2e regression: build a template with `ADD plugin github.com/.../DSH-better-sidebar`
   and assert that the built plugin bundle has NO `require("clsx")`.
4. Bundle Linux deb/rpm and replace the user's `/usr/lib/dshbox/server/linux-x64/dshboxd`.
5. (Backlog) UI built-template badge + dedicated form column; container-path
   parser; batch pnpm hook.