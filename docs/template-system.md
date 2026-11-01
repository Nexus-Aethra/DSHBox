# Template system

Status: current model, 2026-08-20. Detailed contracts live in [prepared-template-runtime.md](specs/prepared-template-runtime.md).

## Terms

- **Prepared base**: an immutable Harness source/dependency cache. Its final-path `node_modules` is used only to seed and validate offline installation; it is never recursively copied.
- **Sealed template**: a physical source recipe derived from a prepared base, containing no `node_modules` and recording its local plugin artifacts.
- **Container**: the final, independently prepared copy of a sealed template. It is independent of the base and all other containers; it uses repository artifacts only during its one-time preparation.

## User flow

1. Pull a root Harness ref. Box clones it to staging, installs dependencies, builds it, runs a bounded health check, and publishes a prepared base.
2. Import extensions into the repository. Each import is converted to a checked local `artifact.tgz`; source preparation remains subject to approval.
3. Build a Boxfile. Box copies prepared source without `node_modules`, records its local artifact references, and publishes a sealed recipe.
4. Create a container. Box copies that recipe to the final instance path, runs `pnpm install --offline`, adds each local artifact, runs `pnpm run build`, then writes metadata.
5. Start it. Box invokes bundled `pnpm dsh web` from the already prepared container. It does not perform `pnpm install`, plugin add, or `pnpm run build`.

Pull and first container creation are intentionally expensive and visible. Ordinary startup is intentionally boring: process launch and readiness check.

## Template identity and safety

Template identity is digest-addressed and includes the exact base revision, bundled toolchain versions, plugin artifact digests, and materialized profile. All publish operations use a private staging directory followed by atomic rename. A failed task cannot create a visible template.

The reusable trees are source-only and link-free. Windows pnpm workspace junctions are created only during the final instance's offline install, so they cannot point back to staging, a repository cache, or another container.

## Container environment

A boxfile may declare `ENV key=value` lines. They are parsed with the rest of the
script (a key that is not `[A-Za-z_][A-Za-z0-9_]*` is rejected at parse time, and
the last declaration of a key wins), carried into the sealed template's `env`
field, and read back from the container's metadata when the host is started.
`EnvironmentPolicy::declared_env` applies them on top of the clean-room
whitelist, so a declared value beats what the host would have inherited while
Box's own variables — `DSH_HOME`, the loopback port and capability token, the
toolchain paths — are applied after and cannot be redirected by a boxfile. The
point is that a container's environment is a property of its template rather
than of whichever shell happened to start the daemon.

## Skills inside a container

A container's DSH host runs with `DSH_HOME` pointed at the container's
`profile/` directory (`lifecycle.rs` sets it with `task_override`), so skills
live at `<container>/profile/skills/<name>/SKILL.md` and DSH's Cordis loader
reads them from `$DSH_HOME/skills/` at host start. An agent inside the
container therefore sees that skill in its catalogue with no user setup.

Two things put a skill there.

**1. The bundled guide skill.** `write_boxfile_guide_skill`
(`dshboxd/src/containers.rs`) writes `BOXFILE_GUIDE_SKILL` to
`profile/skills/dshbox-guide/SKILL.md` — the name is
`BOXFILE_GUIDE_SKILL_NAME`, not the older `boxfile-guide`. It is written only
by `create_dsh_container_sync`, i.e. the create-from-a-DSH-version path; the
sealed-template path (`create_container_from_sealed` in `sealed.rs`) does not
write it. A pre-existing `SKILL.md` is left untouched, so a user's edits survive
and are never clobbered.

**2. A skill installed per container.** This is the only path that exists today;
`ADD skill` in a boxfile does **not** reach it (see below). The install lands
through `install_container_skill` (`dshboxd/src/bundles.rs`):

| Surface | Command |
|---|---|
| Import into the repository | `dshbox plugin import <dir-with-SKILL.md>` |
| Install a repository entry into one container | `dshbox plugin install <container-id> <repository-entry-id>` |
| Same, via the daemon | `enqueue_container_extension_copy` → `link_repository_extension` |
| Bundle load | `dshbox bundle load <archive> [--conflict keep\|overwrite]` |
| UI | Resources → the entry → copy into a container |

There is **no** `dshbox skill install` / `dshbox skill rm` verb; the
`plugin` group covers skills, and `plugin ls` prints the kind column.

What the install actually does:

- **Detection** — `detect_extension_kind` (`box-extensions/src/lib.rs`) reads
  `SKILL.md` first and returns `ExtensionKind::Skill`; only otherwise does it
  look for a `package.json` declaring `dsh.bundle.patch` (a plugin).
- **Name** — `skill_name` (`dshboxd/src/bundles.rs`) takes the `name:` field of
  the SKILL.md frontmatter (quotes stripped); a missing field is an error.
  `is_safe_identifier` then requires letters, digits, dots, dashes or
  underscores (plus the literal `latest`).
- **Destination** — `profile/skills/<name>/`, moved (not copied) from the
  staging directory, then recorded in `state/extensions.json` via
  `write_extension_record`.
- **Conflict** — an existing destination is an error
  (`skill already exists: <name>`) from `install_container_skill`. Only the
  bundle path offers `--conflict keep` (skip) or `overwrite` (remove first);
  every other path refuses, so two skills of one name can never land on one
  directory silently.

`ADD skill` is parsed by `parse_script` (`box-image/src/script.rs`,
`AddKind::Skill`) and the `box-image` manifest/archive path
(`compile_manifest`, `ResolvedAdd`) can describe a skill add — but that
pipeline only feeds script preview and `.dshimage` archives. The production
sealed manifest (`box-foundation::TemplateManifest`) has no `adds` list at
all: it records `plugin_artifact_ids` and `plugin_sources` only.
`build_sealed_template_from_script` (`sealed.rs`) rejects every non-plugin
op with "sealed-template build currently supports ADD plugin only; import
skills/data after the container-copy migration". So a boxfile advertises a
directive the build refuses — use the repository install path above.
(Text that predates the pivot still shows it: `README.md`'s minimal
example lists `ADD skill team-conventions` uncommented, `examples/boxfile.dsh`
line 51 is a live `ADD skill` (so building that file fails at the parser-to-
builder hand-off), and README's "how each `ADD` is stored" claim that skills
and data are snapshotted into the data store, materialised in the template and
copied into the profile describes the retired metadata-only path. The template
`dshbox init` writes keeps every `ADD skill` commented out.)

Unlike a plugin, a skill is not a Cordis plugin: it never appears in the
plugin dependency graph, is never loaded as a service, and ships per container
— rebuilding the Harness does not retouch it. A container's skills can be
inspected with `dshbox container describe <id>`, whose `extensions.skills`
list comes from `scan_container_extensions` (`box-extensions/src/lib.rs`).

## Breaking storage change

This model requires storage schema 10. Box does not support or mutate legacy metadata-only templates and `runtimes/<version>/source` directories. Users of an old data root must choose a new root after preserving any data they need.
