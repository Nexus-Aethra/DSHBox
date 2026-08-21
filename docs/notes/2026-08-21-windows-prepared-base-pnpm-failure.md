# Windows prepared-base dependency-install failure

## Status

Resolved in the installed Windows daemon on 2026-08-21. After a clean-store
failure reproduced the missing Windows optional packages, a real CLI task using
the desktop application's `D:\ddd` runtime directory completed the clone,
dependency install, validation, and prepared-base publish steps with the
platform-scoped fix. The completed task was
`f196ceea-483c-4771-870b-1066d2985543`.

Earlier task `a5255b71-3adf-44b0-ac33-4b18e7579d93` proved the diagnostic
`--force` path only. It is not acceptance evidence for the release path because
it downloaded all platform artifacts.

## Observed failure

The template-pull task cloned the Harness repository, then failed during the
prepared-base `pnpm install` step. The relevant transcript showed both:

- `esbuild` could not find `@esbuild/win32-x64` on the filesystem.
- `koffi` could not load a prebuilt Windows binary, fell back to compiling from
  source, and then failed because CMake was unavailable.

Those are consequences of one primary condition: pnpm did not materialize the
lockfile's optional platform packages on Windows. The CMake failure is only a
secondary fallback failure.

## Rejected primary fix: bundle CMake

Bundling CMake can make one native package's source fallback possible, but it
does not repair `esbuild`'s missing platform package. It also increases the
installer size and makes each template pull depend on a local native-toolchain
build. Do not use it as the primary recovery path.

## Chosen fix

Prepared-base installation invokes:

```text
pnpm --config.optional=true install
```

The daemon also replaces inherited `npm_config_optional` and
`pnpm_config_optional` values with `true`. This preserves pnpm's normal,
platform-aware optional dependency selection. Before installation it appends a
`supportedArchitectures` entry for the current host to the private staging
clone's `pnpm-workspace.yaml`. This makes pnpm materialise Windows x64 esbuild,
koffi, and lefthook prebuilds without selecting Android, macOS, or Linux
artifacts. A separate isolated install confirmed that shape (935 packages and
the required Windows x64 native packages).

`pnpm --force` was used only as a diagnostic recovery experiment. It proved
the root cause but downloads every platform's optional artifacts, so it is not
used by the release path.

The release path now also runs pnpm in a Box-owned clean-room environment.
Registry, npmrc files, pnpm home, cache, store, and Windows application-data
paths are fixed below the selected runtime directory. Real task
`125de858-ffe4-45b4-9695-3fd926cc99d0` completed the official Harness pull
using this policy.

## Diagnostics retention

Staging directories are correctly removed after a failed pull, but that used
to delete the `prepare.log` named by the error. The pull failure path now copies
the transcript to:

```text
<runtime-directory>/logs/templates/<task-id>-pull.log
```

before it deletes staging, and returns that stable path in the task error.

## Required acceptance test

1. Build and install the changed Windows package.
2. Ensure the installed `dshboxd` sidecar reports the new build stamp.
3. Run the real CLI command against the same configured runtime directory as
   the desktop app:

   ```powershell
   dshbox pull template github.com/deepseek-ai/deepseek-harness:dsh-v0.1.0-rc.8
   ```

4. Confirm the task completes and that the prepared base validates.
5. Only then repeat from the desktop Harness page. If it fails, inspect the
   retained log path first; do not rely on a deleted staging path.

The CLI portion has passed. The remaining release check is to install the
packaged desktop build and repeat step 5 through the Harness page.

## Non-goals for this fix

- Do not add a system Node, system pnpm, Git, or CMake requirement.
- Do not change the UI into a separate installation implementation. UI and CLI
  must both enqueue the same daemon `pull_template` operation.
- Do not claim a cold-start success based on a cache hit.
