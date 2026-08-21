# Bundled Git Runtime (Windows and Linux)

## Status

Implemented for Windows. Linux support is deferred until DSH Box CI can
produce a private Git bundle (the `git` section of `runtime-lock.json`
contains only `win-x64` today). The architecture below is the design of
record; the runtime-manifest schema, clean-room policy, and packager
helpers are all in place.

## Problem

DSH Box deliberately clears the host environment before running its bundled
Node/pnpm toolchain. This prevents a host `.npmrc`, proxy, `NODE_PATH`, or
system executable from making a task non-reproducible. DSH's official
`plugin add` command delegates Git specifications to pnpm, and pnpm invokes
`git ls-remote` itself. A clean-room task therefore cannot resolve a Git plugin
unless it exposes a Git executable.

Re-adding the user's `PATH` is not acceptable: it would reintroduce all of the
configuration and executable drift this isolation work removed.

## Decision

Ship an integrity-verified Git distribution as a first-class component of the
bundled runtime, beside Node and pnpm. The daemon injects only this distribution
into package-manager children. No child discovers or uses system Git.

The runtime layout becomes:

```text
<install>/runtime/<target>/
  node/
  pnpm/
  git/
    cmd/git.exe                    # Windows entry point
    bin/git                         # Linux entry point
    ...distribution-owned support files...
  LICENSES/
    node-*.txt
    pnpm-*.txt
    git-*.txt
```

At runtime, mutable Git state remains below the user-selected DSH Box storage
root, never beside the application binary:

```text
<storage>/git/
  home/                             # HOME / USERPROFILE for Git children
  config/global.gitconfig           # empty, Box-owned global config
  cache/                            # optional Git HTTP cache only
```

## Distribution inputs

`runtime-lock.json` gains a versioned `git` section. Each entry contains the
immutable source URL, archive format, SHA-256, and an explicit layout adapter:

```json
{
  "git": {
    "win-x64": {
      "source": "git-for-windows-portable",
      "url": "https://github.com/git-for-windows/git/releases/download/<tag>/PortableGit-<version>-64-bit.7z.exe",
      "sha256": "<pinned-sha256>",
      "entry": "cmd/git.exe"
    },
    "linux-x64": {
      "source": "dshbox-ci-built",
      "gitVersion": "<version>",
      "url": "https://downloads.example.invalid/dshbox/git/<version>/linux-x64.tar.zst",
      "sha256": "<pinned-sha256>",
      "entry": "bin/git",
      "glibcBaseline": "2.28"
    },
    "linux-arm64": { "...": "same contract" }
  }
}
```

The final Linux URLs are release assets produced by DSH Box's CI and signed or
checksummed before they are placed in the lock file. They are not URLs to a
developer's distro package repository.

### Windows

Use the official **PortableGit** artifact from Git for Windows. It is designed
to run from an arbitrary directory without registry writes or administrator
installation. The extraction recipe must execute the distribution's required
post-install step when extracting the self-extracting archive manually. The
runtime exposes only `<runtime>/git/cmd` on `PATH`.

Bundle the full portable tree, not just `git.exe`: Git for Windows needs its
MSYS2 runtime, helper programs, SSL/cURL components, and command scripts.

### Linux

Do not copy `/usr/bin/git` from a build machine. It couples the installer to
that machine's loader, OpenSSL, cURL, and glibc versions.

Build an application-private Git bundle in CI from a pinned upstream Git source
tarball. Build once per `linux-x64` and `linux-arm64` in the oldest supported
glibc environment (initial baseline: glibc 2.28). Package Git plus every
non-system shared object it needs under `git/lib/`, then test it with an empty
environment. The launcher sets `LD_LIBRARY_PATH=<runtime>/git/lib` only for
children receiving the managed Git path.

The Linux builder must record the upstream source version, source archive
SHA-256, build container digest, output SHA-256, and SBOM. A CI job verifies
the binary has no dependency on unsupported host paths via `ldd`/`readelf`.

macOS remains out of scope for this phase; adding it requires the equivalent
notarized/universal bundle decision, not a fallback to `/usr/bin/git`.

## Runtime-packager changes

1. Extend the `runtime-packager` manifest parser and target model with optional
   Git inputs and required entry paths.
2. Download to the existing content-addressed package cache, verify SHA-256
   before extraction, then extract atomically into a temporary sibling.
3. Run target-specific post-processing:
   - Windows: portable distribution initialization and `cmd/git.exe` assertion.
   - Linux: assert `bin/git` is executable and its private library layout is
     complete.
4. Write a runtime manifest containing the Node, pnpm, and Git versions,
   source URLs, checksums, entry paths, and license-file paths.
5. Publish the fully prepared runtime directory with atomic rename. On failure,
   retain the downloaded archive and a diagnostic log but never publish a
   partial `git/` tree.
6. Include `runtime-lock.json`, packager source, and the Git license templates
   in the `scripts/prepare-runtime.mjs` cache inputs.

The Tauri per-platform configs must include `resources/runtime/<target>/` as a
single resource tree. This keeps Node, pnpm, Git, manifests, and notices from
drifting apart during MSI/deb/AppImage bundling.

## Process policy

Add `ResolvedBundledRuntime.git: PathBuf` and resolve it from the runtime
manifest rather than hardcoding a platform path. `bundled_package_manager_policy`
then prepends only the managed Git command directory after Node and pnpm.

Git-specific clean-room values are explicit:

```text
GIT_CONFIG_NOSYSTEM=1
GIT_CONFIG_GLOBAL=<storage>/git/config/global.gitconfig
GIT_TERMINAL_PROMPT=0
GIT_ASKPASS=<Box-supplied non-interactive helper, if required>
HOME=<storage>/git/home
USERPROFILE=<storage>/git/home                # Windows
LD_LIBRARY_PATH=<runtime>/git/lib             # Linux only
```

No `GIT_*` setting, proxy setting, credential helper, or PATH segment is
inherited from the host. Credentials for authenticated Git sources are an
explicit future Box configuration surface; the initial release supports public
HTTPS sources only and must fail with a clear authentication message.

## Security and licensing

- Treat Git archives exactly like Node and pnpm: pinned URL + checksum,
  verified before extraction, no silent download fallback.
- Upgrade Git promptly on upstream security releases; the runtime manifest
  makes installed versions observable in diagnostics.
- Set `GIT_CONFIG_NOSYSTEM=1` and disable interactive prompts to prevent a
  malicious repository or machine config from changing behavior.
- Keep `GIT_SSL_NO_VERIFY` unset and reject an attempt to configure it.
- Include Git's GPLv2 license, Git for Windows notices on Windows, and the
  complete notices/SBOM for bundled Linux libraries in the installer.
- Do not execute Git hooks during Box's resolution steps. Git is used only for
  remote metadata/fetch operations owned by pnpm/DSH.

## Rollout plan

1. **Manifest and packager:** add target entries, archive verification,
   extraction, runtime manifest, and license copying. Do not change daemon
   PATH yet.
2. **Resolver and clean-room policy:** resolve the packaged entry point;
   inject only managed Git plus Git-specific state variables. Add unit tests
   proving host Git and host `.gitconfig` cannot leak.
3. **Windows release:** add PortableGit x64 to the MSI and run the Boxfile
   regression below on a Windows machine with `git.exe` removed from PATH.
4. **Linux release:** publish CI-built x64 and arm64 bundles, then validate on
   two supported distributions per architecture where system Git is absent.
5. **Remove temporary workarounds:** delete any host-Git inheritance or
   documentation asking users to install Git manually.

## Required acceptance matrix

| Platform | Preconditions | Required result |
|---|---|---|
| Windows x64 | `git.exe` absent from PATH; clean selected storage root | `dshbox build boxfile` with `ADD plugin github.com/omdsh-dev/DSH-better-sidebar:v0.14.0` succeeds. |
| Windows x64 | Host `.gitconfig` contains a sentinel proxy/helper | Task environment does not observe the sentinel. |
| Linux x64 | No system Git installed | Same Git-backed Boxfile build succeeds. |
| Linux arm64 | No system Git installed | Same Git-backed Boxfile build succeeds. |
| Windows + Linux | Invalid Git archive checksum | `runtime:prepare` aborts before publishing runtime resources. |
| Windows + Linux | Public Git source unavailable | Error identifies managed Git network failure; no host Git fallback occurs. |

## References

- Git for Windows documents PortableGit as a no-registry, no-admin portable
  distribution and documents using its `cmd/` directory for `git.exe`:
  [PortableGit README](https://github.com/git-for-windows/build-extra/blob/main/portable/root/README.portable).
- Git for Windows releases include Portable Git artifacts and track upstream
  Git security releases: [Git for Windows security policy](https://github.com/git-for-windows/git/security/).
