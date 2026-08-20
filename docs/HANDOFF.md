# DSH Box handoff

Updated: 2026-08-20

## Current decision

The project has approved a forward-only storage and lifecycle pivot after
Windows container creation repeatedly failed during pnpm installation/build in a
shared Harness tree. The authoritative target is
[prepared-template-runtime.md](specs/prepared-template-runtime.md):

```text
pull root ref → prepare base (install + build + validate)
Boxfile build → seal full template (local plugin artifacts installed)
create container → physical copy only
start container → pnpm dsh web only
```

There is no old-layout compatibility requirement. Do not implement a fallback
that reads, repairs, or starts from `runtimes/<version>/source`, metadata-only
templates, or shared profiles. Never auto-delete an existing old data root;
block it and require the user to select a new one after preservation/backup.

## Why the pivot is required

Observed Windows desktop runs failed at different points inside `pnpm install`
while materializing the same shared `node_modules` tree. A retry can move the
failure but does not make the architecture deterministic. The official Harness
source flow prepares artifacts before `pnpm dsh web`; DSH Box must do that once
for a base and never make a newly created container perform that work.

The earlier port allocation/readiness improvements remain useful, but they are
not a solution to install/build failures. Port allocation occurs immediately
before host spawn, with bounded bind/readiness retries.

## Implementation order

1. Implement schema-10 paths, manifests, resource references, staging, and
   atomic publish helpers.
2. Make root template pull prepare/validate an immutable full Harness base.
3. Make repository plugin import publish immutable local tarball artifacts.
4. Make Boxfile build copy source without `node_modules`, retain local artifact
   references, and publish a sealed recipe.
5. Make container creation copy that recipe to its final path, run offline
   install, add local artifacts, and build there; ordinary container start must
   remain package-manager-free.
6. Update RPC/UI/CLI task stages and resource views together; the two entry
   points must be behaviorally identical.
7. Add the cross-platform verification matrix and only then rebuild the Windows
   MSI for manual UI validation.

## Current code-state caution

The worktree contains unrelated and in-progress implementation changes. Keep
them unless they overlap the schema-10 migration. Existing Windows retry and
host-port changes were appropriate diagnostics/mitigations for the former path,
but implementation should now move the expensive work into base preparation and
template sealing rather than extend launch-time recovery logic.

## Required manual acceptance run

Using a fresh selected runtime root on Windows and Linux:

1. Pull `github.com/deepseek-ai/deepseek-harness` and see install/build only in
   the prepare-base task.
2. Build/start a zero-plugin template with no network required after preparation.
3. Import DSH-better-sidebar, build a sealed template, start it, and verify the
   UI plugin loads.
4. Repeat through CLI and the packaged MSI, comparing task stages and logs.
5. Confirm an old root is safely rejected rather than migrated or deleted.
