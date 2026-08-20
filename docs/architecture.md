# DSH Box architecture

## System boundary

DSH Box consists of a React management UI, a Tauri shell/CLI, and the
`dshboxd` sidecar. The daemon is the sole writer of resource state and the
scheduler owner. Desktop and CLI submit the same RPC-backed work and consume
the same resource/task events; neither owns a second lifecycle implementation.

```text
React UI / dshbox CLI
          |
          v
 Tauri adapters / HTTP client
          |
          v
 dshboxd: scheduler + state + lifecycle
          |
          +-- libgit2 checkout
          +-- bundled Node/pnpm process execution
          +-- resource and diagnostics persistence
```

Only the desktop package depends on Tauri. Crate dependencies flow from
foundation/runtime/scheduler/state toward functional crates and then adapters.

## Prepared, sealed, and running state

The managed data root is intentionally not a shared DSH runtime. It has three
immutable published resource types:

```text
prepared base  --copy source + artifact references--> sealed recipe
                                                        |
                                                        +--copy, offline install, add, build--> container
```

A prepared base is a single immutable Harness revision for which Box has run
`pnpm install` to seed the local dependency cache. A sealed template is a
physical source recipe (without `node_modules`) containing a Boxfile's profile,
plugin-artifact references, skills, and data. A container is prepared from that
recipe in its final directory with offline install, local artifact add, and the
final frontend build.

This removes the Windows-sensitive relationship where multiple containers or
profiles can observe the same `node_modules` tree, symlink/junction, or pnpm
transaction. pnpm creates links only under the final container path. A running
container starts `pnpm dsh web` from its own Harness copy and does not install,
add plugins, or build dependencies.

## Persistence and publication

`~/.dsh-box/` holds only machine-local settings, notably the selected data root.
All large data is below that root:

```text
state/                         # schema-10 storage/resource/task records
staging/<task-id>/             # incomplete transaction data only
repository/plugins/<digest>/   # immutable artifact.tgz + provenance
templates/base-<digest>/       # prepared base
templates/sealed-<digest>/     # sealed template
instances/container-<id>/      # independent executable copy
logs/
```

Each long operation creates a private staging directory, validates all required
files, writes a manifest/digests, then atomically renames into a published
resource path and commits the resource-map record. A failure cannot create a
visible partial resource and must not mutate a previously published base,
template, artifact, or container.

`resource-map.json` tracks references from sealed templates to prepared bases
and plugin artifacts and from containers to sealed templates. It gates removal
and lets the scheduler perform delayed cleanup safely.

## Plugin boundary

The extension repository is an artifact cache. Its runtime input is an
immutable local tarball, not a source directory or workspace package. A source
plugin that needs preparation still requires user approval for lifecycle code;
once approved it is prepared/packed/verified exactly once. Template build uses
`pnpm dsh plugin --profile <instance/profile> add <artifact.tgz>` during its
one-time final-path preparation. No running container resolves a repository path
or runs a broad pnpm install.

## Networking and host lifecycle

DSH hosts bind only to `127.0.0.1`. The launcher allocates a dynamic port only
immediately before spawn, passes an unguessable per-launch capability token,
probes readiness with bounded retries, and keeps host output in the
container-local log directory. WebView navigation is restricted to that
loopback origin.

## Migration

This is storage schema 10 and is intentionally forward-only. Legacy
`runtimes/`, metadata-only templates, and shared profiles are neither read nor
rewritten. Selecting an old root presents a recovery choice to preserve it and
select a new empty root. Box never deletes user data while detecting the old
layout.

See [prepared-template-runtime.md](specs/prepared-template-runtime.md) for the
implementation sequence and acceptance matrix.
