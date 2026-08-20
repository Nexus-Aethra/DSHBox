# DSH Box handoff snapshot

Updated: 2026-08-20

This snapshot supersedes the earlier metadata-only built-template handoff. The
project is moving to storage schema 10 and a physically materialized lifecycle.
The complete implementation plan is
[docs/specs/prepared-template-runtime.md](docs/specs/prepared-template-runtime.md).

## Non-negotiable runtime contract

- Pulling a Harness root ref creates a prepared base after bundled `pnpm
  install` and dependency-cache validation.
- Building a Boxfile produces a sealed source recipe by copying the base without
  `node_modules` and recording locally cached plugin tarballs.
- Creating a container copies that recipe to its final path, installs packages
  offline, adds local artifacts, and builds frontend assets once.
- Starting a container runs `pnpm dsh web` from the container-local Harness
  copy on loopback.
- Old `runtimes/` and metadata-only layouts are intentionally unsupported; do
  not delete them automatically.

## Repository guidance

Keep `~/.dsh-box/` to small machine-local configuration only. Large data belongs
to the selected root in `state/`, `staging/`, `repository/`, `templates/`, and
`instances/`. Preserve the existing dirty worktree; only schema-10 work should
replace overlapping old lifecycle paths.

CLI, dshboxd RPC, and desktop UI must submit the same scheduled operations and
show the same durable stages. For implementation sequence and test matrix, use
the linked specification rather than the historical pre-pivot notes.
