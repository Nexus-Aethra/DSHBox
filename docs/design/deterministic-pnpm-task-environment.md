# Deterministic pnpm task environment

Status: phase 1 implemented for daemon pnpm/npm tasks, 2026-08-21.

## Problem statement

Harness preparation is a daemon-scheduled operation, but the pnpm process is
not currently hermetic. Its result can vary with machine-local Node tooling,
global `.npmrc` files, inherited `npm_config_*` / `pnpm_config_*` variables,
proxy variables, and a partially populated store.

This produced several misleading symptoms during Windows template pulls:

- pnpm used `registry.npmmirror.com` even though daemon `get_info` reported
  `npmRegistry: null`;
- network resets or slow mirror responses left the package graph incomplete;
- lifecycle scripts then reported missing Windows optional binaries (esbuild,
  koffi, lefthook), obscuring the earlier network/configuration cause;
- a retry could behave differently solely because the previous attempt had
  populated part of the pnpm store.

The task log from `a49fb087-4e57-4e57-8503-1baeb118413f-pull.log` demonstrates
the issue: all 929 fetched packages were reused on the final retry, while the
native optional packages were still not materialised for the lifecycle scripts.

## Existing reusable component

`box-runtime::process::EnvironmentPolicy` is the correct ownership boundary.
Every daemon pnpm invocation already uses it through:

```text
dshboxd::toolchains::pnpm_policy
  -> box_runtime::process::bundled_toolchain_policy
  -> ProcessSpec / NativeProcessRunner
```

The current policy intentionally preserves unknown parent variables. It pins
the Box pnpm store and cache and overrides a few keys, but that preservation
allows machine-global npm/pnpm configuration to influence scheduled work.

## Implementation record

The daemon now uses a clean-room package-manager policy for every pnpm/npm
task. It creates Box-owned configuration and application-data paths below the
selected runtime directory, explicitly selects the configured registry (or
`https://registry.npmjs.org/`), and does not inherit user npm/pnpm/proxy/Node
configuration. Windows `APPDATA` and `LOCALAPPDATA` are explicitly redirected
to the Box runtime because koffi requires them when installing its prebuild.

The policy was verified with real task
`125de858-ffe4-45b4-9695-3fd926cc99d0`: the official Harness pull completed
against `D:\ddd` after writing `D:\ddd\pnpm\config\npmrc` with the explicit
official registry. Earlier failures used the externally inherited npmmirror
registry even while Box configuration reported no selected registry.

## Decision

Introduce a **clean-room package-manager policy** in `box-runtime`. It is
separate from the DSH host policy: host processes may keep normal user-facing
environment values, while package resolution must have a fixed, recorded
input set.

### Invariants

1. The bundled Node and pnpm paths are absolute and are the only executable
   runtime paths.
2. The pnpm store and npm metadata cache are below the selected Box runtime
   directory only.
3. The registry is exactly the Box setting, or the explicit default
   `https://registry.npmjs.org/`; a global `.npmrc` cannot override it.
4. User/global `.npmrc`, `npm_config_*`, `pnpm_config_*`, `NODE_PATH`, and
   system pnpm home/config directories do not influence a task.
5. Explicit Box settings for registry and proxy are the only network settings
   passed through.
6. Each task log records the effective registry, proxy-presence boolean,
   bundled pnpm version, store path, platform, and config file paths. Never
   log credential values.
7. A failed staging task may populate the content-addressed store, but it may
   not publish a prepared base or influence registry/configuration selection
   of the next task.

## Data layout

All package-manager state is Box-owned:

```text
<runtime-root>/
  pnpm/
    store/                 # pnpm content-addressed store
    npm-cache/             # npm metadata/cache
    config/
      npmrc                # generated, private, no credentials
      pnpm-config/         # generated pnpm configuration directory
```

The generated `npmrc` is not a user preference file. It contains only the
effective registry and controlled network settings. It is rewritten atomically
when Box settings change and must be excluded from diagnostics if credentials
are ever added in the future.

## Implementation plan

1. Extend `EnvironmentPolicy` with a clean-room mode that starts from an
   allowlist instead of inheriting the parent environment. Keep the existing
   preserving mode for regular host processes.
2. Add `bundled_package_manager_policy(...)` in `box-runtime`:
   it supplies the Box runtime paths, private store/cache/config paths,
   current platform selection, optional dependencies enabled, and the
   explicit registry.
3. Generate the private npm/pnpm configuration under `<runtime-root>/pnpm`
   using atomic writes in `box-foundation`; point pnpm/npm at it with explicit
   environment variables.
4. Move `dshboxd::toolchains::pnpm_policy` to this new policy. Do not duplicate
   process/environment rules in the daemon, desktop adapter, or CLI.
5. Route every Box-owned pnpm/npm action through it: prepared-base pull,
   template build, plugin add, container finalisation, and any retry path.
6. Add a redacted environment fingerprint to `run_pnpm_task` logs before the
   process begins.
7. Add an explicit recovery view for network failures: report registry host,
   retry count, and the Box setting to change. Do not relabel a failed download
   as a native build-tool failure.

Items 1 through 5 are complete for daemon pnpm/npm tasks. Items 6 and 7 remain
follow-up observability work. Proxy support remains intentionally absent until
it is modelled as a Box setting rather than inherited from the operating system.

## Tests and acceptance criteria

### Unit tests in `box-runtime`

- A parent environment containing `npm_config_registry`, `NPM_CONFIG_USERCONFIG`,
  `PNPM_HOME`, `NODE_PATH`, and proxy variables cannot alter the clean-room
  policy unless the matching Box setting is explicitly supplied.
- The policy pins the selected runtime's store/cache/config paths on Windows
  and Linux.
- Case-insensitive Windows aliases are removed.
- Secrets never appear in the rendered diagnostic fingerprint.

### Daemon integration tests

- Seed a fake global `.npmrc` pointing at an invalid registry; template
  preparation must use the configured Box registry instead.
- Run two preparations with different hostile parent configurations; their
  effective command/environment fingerprints must be identical.
- A failed first download followed by a retry must retain the same registry and
  platform selection; it may reuse verified artifacts only.
- The Windows optional package fixture must materialise the Windows x64
  esbuild/koffi/lefthook packages without fetching other operating systems'
  binaries.

### Manual acceptance

1. Start with a clean Box runtime directory and a deliberately customised
   user `.npmrc`.
2. Pull the official Harness once from the UI.
3. Confirm the log names only the Box-selected registry and shows the
   clean-room fingerprint.
4. Build and start a template from that prepared base without another full
   dependency download.
5. Repeat on Linux with the same Box configuration contract.

## Non-goals

- Do not bundle CMake or a system-wide Node/pnpm installation to compensate
  for dependency-selection or registry failures.
- Do not expose user `.npmrc` or proxy credentials in UI/task logs.
- Do not give UI and CLI independent installation implementations; both must
  execute the daemon's single package-manager policy.
