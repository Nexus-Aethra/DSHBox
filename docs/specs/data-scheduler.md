# Data scheduler specification

Status: schema-10 design, 2026-08-20.

## Single source of truth

`state/resource-map.json` is the authoritative index of published resources.
Task state is persisted separately in `state/task-store.json`; task-private
files live only in `staging/<task-id>/`. Filesystem scans may diagnose drift but
must not silently recreate state records.

The managed resource kinds are:

| Kind | Published path | Direct references |
|---|---|---|
| Plugin artifact | `repository/plugins/<digest>/` | none |
| Prepared base | `templates/base-<digest>/` | plugin artifacts only when retained for provenance |
| Sealed template | `templates/sealed-<digest>/` | prepared base + plugin artifacts |
| Container | `instances/container-<id>/` | sealed template |

The physical content of a container is independent even though its metadata
retains a reference to its source template. References protect source resources
from removal and allow provenance; they are not runtime filesystem links.

## Resource record

```text
ResourceEntry {
  id: String,                 // "plugin:<digest>", "base:<digest>", ...
  kind: ResourceKind,
  path: RelativePath,
  status: Active | Deleted,
  refs: Vec<ResourceId>,      // resources that directly depend on this record
  manifestDigest: String,
  sizeBytes: u64,
  createdAt: u64
}
```

Resource publication is ordered: validate staged tree, write manifest, atomically
rename staging to its digest-addressed final path, then commit map/references.
On startup, an unreferenced staging directory is recoverable cleanup, never a
published resource.

## Delete and cleanup

Removal is `soft-delete → fast queue → slow retry queue`. A resource with live
references cannot be removed. A deletion failure is recorded in diagnostics and
retried by the scheduler without corrupting the map. The daemon never deletes a
runtime root merely because its schema is legacy; schema migration requires a
new root selection.

## Scheduler task classes

All mutating work acquires a resource lock and reports durable stages:

| Task | Required stages |
|---|---|
| Prepare base | clone, install dependencies, build frontend, validate, publish |
| Import plugin | prepare (if approved), pack artifact, validate, publish |
| Seal template | copy base, add local artifacts, validate, materialize links, publish |
| Create container | copy sealed template, write metadata, publish |
| Start host | allocate port, launch, wait ready |

`Create container` must never report an install or frontend-build stage. Those
operations belong to prepare/seal tasks, making CLI and UI failure logs directly
comparable.

## Integrity rules

- Published base/template/container trees contain no external symlink, junction,
  or reparse-point dependency.
- Plugin installation input is the repository's immutable local `artifact.tgz`.
- Digests and toolchain versions are recorded in manifests.
- Retried work uses new staging, never a published or partially materialized
  `node_modules` tree.

The full storage contract is in
[prepared-template-runtime.md](prepared-template-runtime.md).
