# Template system

Updated: 2026-08-17 (corrections: `data` kind is implemented, FROM chain
depth is 4, install flow goes through `pull_template`, the built-template
build pipeline is specified separately in `docs/specs/image-build.md`)

A **template** is DSH Box's unit of container construction. Everything in the
Resources tab is a template — including the "Harness" entry, which is just the
official DeepSeek harness surfaced under a beginner-friendly name. The Resources
tab is intentionally ordered **Harness → Plugins → Bundles → Template**: the
Harness slot is the entry point that generates a base template, and every other
template extends that base.

This document explains how a template works, the `boxfile` / `.dsh` grammar the
parser accepts, and how a template turns into a running container.

## What a template actually is

A template is **a declaration of which files end up where inside a container**.

Concretely, a template is a single text file with one directive per line. The
parser in [`box-image/src/script.rs`](../src-tauri/crates/box-image/src/script.rs)
reads it into an `ImageScript`, the builder in `box-image/src/manifest.rs`
turns every directive into a `ResolvedAdd` (one source-to-destination copy),
and the runtime then materialises those copies when a container starts.

There is no separate "harness" resource type any more. The official harness is
just a template whose `FROM` line points at the canonical DeepSeek repository;
the Resources page labels it "Harness" purely as a UI affordance for new
users. Every installed DSH version auto-generates a matching base template
under `<runtime>/templates/<version>.dsh`, and that template is what a
container is built from.

## The full grammar

A template is a UTF-8 text file. Blank lines and `#`-prefixed comments are
ignored. The directives, in source order:

```text
FROM <ref>             # required, exactly once
PROFILE <name>         # required, exactly once
NAME <name>            # optional, names the built template
VERSION <version>      # optional, defaults to "latest"
LABEL key=value        # optional, repeatable
DEF <name> @<path>     # optional, repeatable

ADD plugin|skill|data <source> [@<dest>]   # one or more
CP <source> [@<dest>]                 # alias for `ADD plugin`
```

`FROM` resolves to either a GitHub short form (`github.com/owner/repo[:tag|@ref]`,
which becomes the harness slot of the build chain) or a local template name
(`web-base`, looked up under `<runtime>/templates/<name>.dsh` and used to
inherit its harness + profile while adding only this template's own ops —
the "incremental" mode).

### Source shapes (`ADD` / `CP`)

`<source>` is parsed in this order:

1. **`://`** present → tarball (`http://`, `https://` are remote; everything else is local)
2. `/` prefix → absolute filesystem path
3. `./` or `../` prefix → path relative to the script's directory
4. `host/owner/repo[:tag|@ref]` with `host ∈ {github.com, gitlab.com, bitbucket.org}` → GitHub clone
5. Bare name → look it up in the local Repository (`name[@version]`, `:version`, or `@scope/name[@version]`)

### Resources at a glance

A template is fundamentally a **plugin aggregator**. The three built-in kinds
today are `plugin`, `skill`, and `data`.

- **plugin** — copied to `@plugin`, which the builder resolves against the
  `DEF plugin` table. The default system DEF is
  `@profile/profiles/<profile>/node_modules`, i.e. the profile's `node_modules`
  tree. Plugins know how to be installed (manifest, hooks, post-install);
  `ADD plugin` is the syntax sugar for that whole flow.
- **skill** — copied to `@skill`, which defaults to `@profile/skills`. Skills
  are data-only resources, so the builder just copies them in place.
- **data** — materialised into the content-addressed data store
  (`<root>/data/<digest>/`, fnv1a64 digest) and copied per-container under
  `extensions/data/<name>`. Implemented in `crates/dshboxd/src/data.rs`
  (`materialize_data_add`); orphaned blobs are reclaimed by `dshbox
  template prune`.
- **session** — reserved for cross-container migration (`ADD session
  container1` would copy another container's session history). Not yet wired
  through the script parser; the manifest schema already supports it via
  `AddSource::ContainerPath`.

`CP <source> [@<dest>]` is exactly `ADD plugin <source> [@<dest>]`. It exists
because the directive table used to have `CP` as a separate keyword that just
copied a file; we now accept either spelling.

### `DEF` and the `@` path language

The destination `@<dest>` (third ADD argument) is resolved against the
template's DEF table:

| Pattern | Meaning |
| --- | --- |
| `@plugin`, `@skill` | look up the named DEF, append `/<rest>` |
| `@profile/...` | literal — the path is already relative to the container profile root |
| `@/abs/path` | literal absolute path inside the container |
| `@<name>` (no sub-path) | exactly the value of `DEF <name>` |
| bare name (no `@`) | error — destinations must start with `@` |

The system provides two built-in DEFs every template inherits (overridable):

```text
DEF plugin → @profile/profiles/<profile>/node_modules
DEF skill  → @profile/skills
```

You add your own with `DEF <name> @<path>`. Once defined, both `ADD plugin
@mybin/foo` and `ADD @mybin/foo` resolve through it.

`ADD` without an explicit `@<dest>` falls back to the kind's default:
plugin → `@plugin`, skill → `@skill`. `data` payloads are stored in the
data store and need no destination at all.

## A complete example

```dsh
# base template — auto-generated for the official DeepSeek harness
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME deepseek-harness
VERSION latest

# DEF plugins into a sibling directory of the profile so we can swap them
# without rebuilding the runtime.
DEF extension @profile/extensions

ADD plugin github.com/team/cordis-plugin-foo:1.2.3
ADD plugin https://intranet/foo.tgz
ADD plugin ./plugins/secret
ADD plugin cordis-plugin-bar@1.0.0
ADD skill team-conventions
CP ./local-config.json @extension/config.json
```

Each `ADD` becomes a `ResolvedAdd` whose `destination` is fully resolved at
build time. The manifest is then content-addressable: every `ResolvedAdd`
carries a `blob` path and `digest`, so the builder skips re-fetches when the
content already exists in the archive.

## From template to running container

1. **Pull** the harness — `dshbox pull template github.com/deepseek-ai/deepseek-harness[:tag]`
   clones the repository into `<runtime>/runtimes/<version>/source` via
   libgit2, writes the completion marker `.dshbox-runtime.json` (the
   installed-version criterion), and registers a base template in the
   content-addressable template store (`templates/<hash>/script.dsh` +
   `state/template-index.json`; a legacy `templates/<version>.dsh` alias is
   kept for the build path).
2. **Extend** — the user runs `dshbox init` or copies the base template and
   adds `ADD` / `DEF` lines.
3. **Build / run** — `dshbox build` produces a metadata-only **built
   template** (resolving the `FROM` chain up to **four** levels): plugins
   are recorded as references into the shared repository, every other kind
   is hashed into the data store as a snapshot, and the result lands in the
   same content-addressable store as script templates. `dshbox run
   <template>` then creates and starts a container from either form — a
   built template materialises from its resource list, a script template is
   parsed and materialised live. An optional `--output` exports a portable
   `.dshimage` archive. Full design: `docs/specs/image-build.md`.
4. **Start** — the daemon creates the container directory, links/copies the
   materialised extensions into the profile, writes the `dsh-box-context`
   snapshot + patch, and spawns the DSH Host against that profile.

## Resources page → templates

The Resources tab in the UI reflects this model directly:

- **Harness** — the tag catalog of the official harness repo (fetched over
  the git protocol) plus installed DSH versions. Pulling a tag registers a
  base template in the template index. The "Fill missing templates" button
  calls `upgrade_legacy_resources`, which scans `<runtime>/runtimes/`,
  ensures every installed version has a base template, and returns a
  per-version report (`{version, templatePath, templateCreated}`).
- **Plugins** — entries that live in the local Repository (`<runtime>/repository/plugins/`).
  Pull one into a template with `ADD plugin <name>`.
- **Bundles** — saved selections of plugin entries; useful for sharing a
  curated toolchain across containers.
- **Template** — the local templates directory (both forms live in one
  index: script templates and the built templates `dshbox build` produces).
  You can pick a `.dsh` file, preview its operations, and queue a build.

When you click the "Harness" tab, you are looking at templates; the Resources
page is the only place where the older "harness" terminology survives, and
only as a label.

## Migration notes (for older installations)

- `ADD data <source>` is fully supported (see "Resources at a glance");
  earlier drafts of this document incorrectly said it was removed.
- `CP` is accepted as a keyword aliasing `ADD plugin`. Old scripts that
  wrote `CP ./local.conf @profile/etc` continue to parse unchanged.
- `.dshbox-runtime.json` IS live metadata: `pull_template` writes it after a
  successful clone and both `installed_versions` and container creation use
  it as the install-completion marker (a `.git` directory alone means a
  clone is still in flight). Only the legacy `<runtime>/runtimes/<v>/.dboxfile`
  file is a dead artefact. The migration pass ensures every installed
  version has a base template.
- `ResourceKind::Harness` has been removed. UI rows that previously reported
  `kind: "harness"` will simply not appear; the corresponding `runtime:<v>`
  resource continues to exist.