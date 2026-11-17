# Data scheduler specification

Status: schema-10 design, 2026-08-20. The implementation that carries this
contract now lives in the `box-state` crate
(`crates/domain/box-state/src/{resource_map,deletion_queue,deletion_worker}.rs`);
the crate that used to be named `box-data-scheduler` was merged into it
because it had exactly one consumer.

## Single source of truth

`state/resource-map.json` is the authoritative index of published resources,
and it is still a plain JSON file written atomically through
`resource_map_path` / `write_resource_map` (`box-state/src/resource_map.rs`).
It has not migrated to the document store: `LEGACY_SCOPES` in `box-store`
(`box-store/src/import.rs`) lists only `tasks`, so AGENTS.md's document-store
rule describes the direction of travel, not today's layout — this spec
describes today's. Task state is persisted in the document store
(`state/dshbox.db`; the legacy file was `state/tasks.json`); task-private
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
  type: ResourceType,         // Plugin | Template | Container | Unknown;
                             // spelled r#type in Rust, `type` in JSON
  path: String,               // absolute path of the published tree on disk
  status: Active | Deleted,
  refs: Vec<String>,          // resource ids that directly depend on this record
  meta: BTreeMap<String, String>,   // free-form per-entry extras
  created_at: u64             // written as createdAt in JSON (rename_all = "camelCase")
}
```

Only `id`, `type` and `path` are supplied by callers:
`ResourceEntry::new` (`resource_map.rs`) fills in `status`, `refs`, `meta` and
`created_at` itself. `manifestDigest` and `sizeBytes` are not recorded here at
all — no caller writes them into the map. `meta` is likewise empty on every
entry in practice: the one place that assembles keys for it
(`harness_ref`, `from_ref`, in `box-template-core/src/lib.rs` on template
install) never attaches them to the entry, so today the map holds only
id/type/path/status/refs/created_at. The digest and size figures live on the
published tree instead: `manifest.json` beside the resource
(`TemplateManifest`, written by `write_template_manifest` in
`box-foundation/src/lib.rs`, carries `harnessDigest`, `profileDigest` and
`sizeBytes`), and the per-domain index records carry `sizeBytes`
(`PreparedBaseRecord`, `SealedTemplateRecord`) — `sealed.rs` reports the
latter over RPC as `sizeBytes`.

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

`Create container` normally reports no install or frontend-build stage: those
operations belong to prepare/seal tasks, which is what makes CLI and UI failure
logs directly comparable. The exception is a sealed template whose tree carries
no artifacts for this commit and platform — an old template, or one imported
from another OS, whose native addon is not this machine's. That one is rebuilt
in place, and its task log says which path it took.

## Integrity rules

- Published base/template/container trees contain no external symlink, junction,
  or reparse-point dependency.
- Plugin installation input is the repository's immutable local `artifact.tgz`.
- Digests and toolchain versions are recorded in manifests.
- Retried work uses new staging, never a published or partially materialized
  `node_modules` tree.

The full storage contract is in
[prepared-template-runtime.md](prepared-template-runtime.md).
