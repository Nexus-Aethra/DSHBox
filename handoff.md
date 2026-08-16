# DSH Box — Handoff Document

> NOTE: This document is a historical snapshot. The authoritative design for
> the image build pipeline is `docs/specs/image-build.md`. Current status:
> 98+ Rust workspace tests pass, `dshboxd` compiles and ships as the daemon,
> Windows named pipes are implemented, and ADD data / FROM chaining /
> template import-export are all implemented.

## Current State Summary

**Verification**: cargo test --workspace passes | tsc --noEmit passes | e2e scripts in scripts/ pass

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
| dshbox build | build.rs | Done (produces a metadata-only image) |
| dshbox run | run.rs | Done (image-first, template fallback builds implicitly) |
| dshbox container | container.rs | Done (logs/url/start/stop/rebuild) |
| dshbox template | template.rs | Done (ls/show/import/export/rm) |
| dshbox plugin | plugin.rs | Done |
| dshbox bundle | bundle.rs | Done |
| dshbox image | image.rs | Done (ls/show/rm/prune against the local registry) |
| dshbox config | config.rs | Done |

Removed: `dshbox dsh` (subcommand dropped), `dshbox resources` (superseded by plugin/bundle).

> Build model: `dshbox build` produces a metadata-only image (plugins
> referenced from the repository, every other kind hashed into the data
> store as snapshots); containers are created from images via `dshbox run`.
> Full design: `docs/specs/image-build.md`.

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
(depth limit 4); Windows named pipes are implemented.

Remaining:

1. UI image surface — the CLI image registry is complete, but the desktop
   UI has no image list view yet (containers are still created from
   templates in the dialog); see `docs/specs/image-build.md` section 8
2. Container-path sources — [@]<container-id>@/path parser support is partial
3. Batch pnpm hook — pnpm dsh plugin add is called per-plugin; could batch into one call

---

## Run Commands

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

# Dev server (requires Tauri)
cd src-tauri && cargo tauri dev
