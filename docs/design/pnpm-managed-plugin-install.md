# pnpm-managed plugin recipe installation

Status: current design, 2026-08-20. This replaces the former design that ran pnpm dependency installation in a container against a shared Harness runtime.

## Decision

`ADD plugin <source>` is forwarded to the official DSH pnpm wrapper during template build. The sealed template records the source expression and the profile package/lock recipe which the command produced:

```text
pnpm dsh plugin --profile <profile> add <source>
```

This preserves pnpm's supported source formats (registry packages, aliases, Git, hosted shortcuts, tarballs, and supported path forms) rather than maintaining a second Box source resolver. A running container never runs installation or resolves a repository source.

## Cache and recipe contract

`<runtime-root>/pnpm/store` is the content-addressed pnpm store exclusively owned by DSH Box; `<runtime-root>/pnpm/npm-cache` holds associated metadata. Every Box pnpm invocation receives pnpm 11's inherited `PNPM_CONFIG_STORE_DIR=<runtime-root>/pnpm/store` setting through the shared `EnvironmentPolicy`, including pnpm processes spawned by the official DSH wrapper. Neither system pnpm state nor a user's unrelated project cache participates.

The sealed template retains only the official profile `package.json`, `pnpm-lock.yaml`, workspace settings, and DSH bundle list. It has no profile `node_modules`. The repository remains a UI/provenance index and optional import/export cache, not the package installation authority.

## Build behavior

The builder copies prepared source without `node_modules`, creates an isolated profile recipe using DSH's own command, and records the original source expressions in the sealed-template manifest. A prepared base holds one stable tool-only `node_modules` graph so additional Boxfiles do not re-materialize the full Harness workspace. This graph is never copied.

After the recipe reaches its final container directory, Box runs `pnpm install --offline --frozen-lockfile` in the profile and performs the final frontend build. This final-path order is required because Windows pnpm workspace junctions contain absolute paths.

If install, artifact add, or build fails, only the unpublished instance is discarded. Neither the prepared base nor a published template is modified. Retrying gets a fresh instance rather than reusing a possibly damaged `node_modules` directory.

## Security and provenance

The bundled, integrity-verified Node/pnpm runtime is used through absolute paths. The original source, resolved lockfile, source ref/commit, and lifecycle approval are retained in diagnostic logs and template metadata. Git-hosted packages that require `prepare` remain blocked until the user approves pnpm's exact `allowBuilds` entry; caching never bypasses this check.
