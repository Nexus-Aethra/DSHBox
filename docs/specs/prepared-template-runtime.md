# Prepared template runtime specification

Status: approved implementation plan (2026-08-20)

## Decision

DSH Box will use a three-stage, final-path assembly model:

1. A root-template pull creates a **prepared base**: an immutable checked-out Harness source tree and a seeded local dependency cache. It is a cache for cloning source, not a distributable `node_modules` tree.
2. A Boxfile build creates a **sealed template recipe**: a copied Harness source tree (excluding `node_modules` and VCS metadata), the Boxfile, profile selection, and the profile metadata/lockfile produced by the official `dsh plugin add` command.
3. Container creation copies that recipe into its final instance directory, then uses the bundled package manager and the Box-owned pnpm store to materialize the locked profile offline and build the frontend. Starting an already prepared container only launches DSH.

This replaces the shared `runtimes/<version>/source` model. It is deliberately a storage-schema break: old runtime directories are not supported or migrated in place.

## Goals and boundaries

- Match the official source workflow once per prepared base: `pnpm install`, `pnpm run build`, then `pnpm dsh web`.
- Make a sealed template reproducible from its manifest and local artifacts.
- Keep all plugin code local to the user-selected DSH Box data root.
- Make CLI and desktop use the same scheduler tasks, state machine, commands, paths, and diagnostic logs.
- Make container startup independent of registry availability, package-manager work, shared Junctions/symlinks, and a mutable base runtime.

The cost is intentional per-container preparation work and disk duplication. Box records `sizeBytes` on prepared bases, sealed templates, and containers so the UI can show the consequence before creation. VCS metadata (`.git`), `node_modules`, and transient caches are excluded from reusable source trees. pnpm creates its workspace links only in the final directory they target.

## Canonical storage layout

```text
<runtime-root>/
  state/
    storage.json                 # schemaVersion: 10
    resource-map.json            # template/plugin/container references
    task-store.json
  staging/
    <task-id>/                   # private, disposable, never a launch source
  pnpm/
    store/                       # Box-owned shared pnpm content store
    npm-cache/                   # Box-owned registry metadata cache
  repository/                    # UI/index metadata; never installation authority
  templates/
    base-<base-digest>/
      manifest.json              # kind: prepared-base
      harness/                     # source/build cache; not copied with node_modules
    sealed-<template-digest>/
      manifest.json              # kind: sealed-template
      boxfile.dsh
      harness/                     # recipe source, without node_modules
      profile/
      skills/
      data/
  instances/
    container-<id>/
      container.json
      harness/
      profile/
      skills/
      data/
      logs/
```

`~/.dsh-box/` continues to contain only machine-local configuration pointing to `<runtime-root>`; it must not contain a Harness checkout, pnpm store, plugins, or container data. `runtimes/`, `current/`, and container links into a shared runtime are removed from the schema.

Reusable source trees contain no `node_modules` graph. pnpm may create junctions/symlinks inside an instance during its offline install, but their targets must remain beneath that same final instance directory. A container may never launch files outside its own directory.

## Manifests and resource references

Every published base and template has a content digest, an atomic manifest write, and a `resource-map.json` entry. A sealed-template manifest includes:

```json
{
  "schemaVersion": 10,
  "kind": "sealed-template",
  "id": "sealed-<digest>",
  "base": { "id": "base-<digest>", "ref": "...", "commit": "..." },
  "toolchain": { "node": "...", "pnpm": "..." },
  "pluginSources": ["git+https://github.com/owner/plugin#commit"],
  "digests": { "harness": "...", "profile": "..." },
  "sizeBytes": 0,
  "validatedAt": 0
}
```

Container metadata records the sealed template id and digest it was copied from, but container execution reads only the container-local copy. The resource map uses those references to prevent deletion of a base or plugin artifact that is still needed to rebuild a sealed template; deletion remains scheduler-owned.

## Lifecycle operations

### Pull and prepare a root template

`template pull <harness-ref>` performs one scheduled transaction:

1. Clone the exact revision into `staging/<task-id>/harness` using libgit2.
2. Run bundled pnpm in that directory: `pnpm install`. This seeds the Box-owned store but does not build the frontend.
3. Verify the source and dependency cache.
4. Compute manifest/digests, rename staging atomically to `templates/base-*`, and commit state only after the rename succeeds.

Failure removes only its staging directory and leaves the previous published base untouched. Retrying begins with a clean staging directory; it does not mutate a published base or share a partially written `node_modules` tree.

### Build a sealed template

`dshbox build <boxfile>` copies a selected prepared base excluding `node_modules`. For each `ADD plugin <pnpm-source>`, it asks the official command to resolve the package once:

```text
pnpm dsh plugin --profile <recipe-profile> add <pnpm-source>
```

The generated `package.json`, `pnpm-workspace.yaml`, DSH profile manifest, and `pnpm-lock.yaml` become the sealed recipe; its temporary `node_modules` is removed before publication. A prepared base owns one stable tool graph solely to run this command. That graph is never copied into a template or container.

Git/plugin lifecycle scripts remain an explicit approval boundary. If pnpm reports an `allowBuilds` key, Box must present the exact package and resolved ref for user confirmation before retrying; it must not silently permit it.

### Create and start a container

`container create` copies one sealed recipe to an instance staging directory, moves it to the final instance path while still unpublished, runs the Harness install and `pnpm install --offline --frozen-lockfile` in the copied profile, runs `pnpm run build`, writes `container.json`, validates that link targets are instance-local, and then commits the container record. The observable task stages are `copying template`, `installing DSH dependencies`, `materializing cached plugin recipe`, `building DSH frontend`, and `writing container state`.

`container start` allocates its loopback port immediately before spawn and runs the bundled pnpm command from `<instance>/harness`. It uses the local profile and has the normal bind/readiness retry policy, but it never alters the sealed template, prepared base, repository cache, or another container.

## Migration and recovery

Schema 10 does not read legacy `runtimes/`, shared profile, or metadata-only template layouts. On selecting an old data root, Box displays a blocking storage-upgrade recovery view: choose a new empty root or back up/export the old root and choose a new root. It never auto-deletes, rewrites, or silently uses old data. This is a forward-only product migration, not runtime backward compatibility.

Interrupted tasks are recovered by inspecting the task record and staging directory. Only a fully renamed directory with a valid manifest is visible as a resource. Published bases, sealed templates, and containers are immutable; replacement creates a new digest-addressed resource.

## Implementation plan

1. Add schema-10 path types, manifests, resource kinds, and staging/publish helpers in `box-foundation`, `box-state`, and `box-runtime`.
2. Replace root template pull with the prepared-base transaction in the daemon; move dependency/build/health diagnostics from container startup to this task.
3. Change repository import to produce and validate `artifact.tgz`; preserve approval and integrity checks.
4. Replace metadata-only image/template build with a sealed source recipe and artifact references.
5. Replace container creation with final-directory offline install, local artifact add, and frontend build; remove shared-runtime mutation and all work from ordinary container start.
6. Route CLI, daemon RPC, and desktop UI through the same named task stages and expose size, source digest, and preparation logs.
7. Update removal/reference protection, export/import, and garbage collection for base, sealed-template, plugin-artifact, and container resources.
8. Remove legacy paths and tests after schema-10 behavior is covered; do not retain a compatibility execution path.

## Required verification matrix

- Linux and Windows: pull official Harness template, prepare successfully, build a zero-plugin sealed template, create/start it without pnpm work.
- Linux and Windows: build DSH-better-sidebar directly from its Git source, explicitly approve its pnpm lifecycle build if requested, create/start it, and confirm the plugin is loaded.
- Repeat build and creation to prove source bases/templates remain unchanged; confirm two containers have distinct physical files.
- Disconnect network after preparation and prove create/start still succeeds.
- Interrupt every transaction phase and verify no partial published resource is listed; retry from clean staging.
- Exercise the same cases through CLI and packaged Windows MSI UI and compare task stage names, state, logs, and final manifests.
- Attempt an old root and confirm it is blocked without data deletion.
