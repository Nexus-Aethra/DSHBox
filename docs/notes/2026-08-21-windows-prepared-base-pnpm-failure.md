# Windows prepared-base dependency-install failure

## Status

Resolved in the installed Windows daemon on 2026-08-21. A real CLI task using
the desktop application's `D:\ddd` runtime directory completed the clone,
dependency install, validation, and prepared-base publish steps. The completed
task was `a5255b71-3adf-44b0-ac33-4b18e7579d93` and produced
`templates/base-620592ceac885222`.

Previous successful pulls remain insufficient evidence on their own because
they may have reused an already-populated pnpm store or a previously prepared
base. The above task was initiated after the failure and used the replacement
installed sidecar.

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
platform-aware optional dependency selection: a Windows x64 host receives the
Windows x64 esbuild and koffi prebuilds, but not Android, macOS, or Linux
artifacts. A separate isolated install confirmed that shape (935 packages and
the required Windows x64 native packages).

`pnpm --force` was used only as a diagnostic recovery experiment. It proved
the root cause but downloads every platform's optional artifacts, so it is not
used by the release path.

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
