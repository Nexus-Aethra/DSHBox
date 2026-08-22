# Linux host-git passthrough — handoff notes

This note explains the new clean-room host-git fallback mode for Linux so the
Linux-side team can validate the behaviour and plan when (if) they want to
replace it with a CI-built bundled Git.

## Background

DSH Box's clean-room package-manager policy strips the host environment
before invoking pnpm. pnpm in turn spawns `git ls-remote` to resolve
`ADD plugin github.com/...` specs. Until now, `git` was required to be
bundled next to Node and pnpm under `<runtime>/git/`. The Windows
distribution ships Git-for-Windows PortableGit; the Linux distribution
shipped nothing because no portable Linux Git bundle exists (every
distro's `/usr/bin/git` is linked against the host's glibc, OpenSSL,
and cURL, so copying it is not portable).

## What shipped in this PR

A new `host_git_dir: Option<&Path>` parameter on
`bundled_package_manager_policy`. The daemon's
`pnpm_policy` (in `dshboxd/src/toolchains.rs`) now:

1. Reads `bundled_runtime().git_dir` first (today `None` on Linux
   because `runtime-lock.json` has no `linux-x64` git entry).
2. If `None` AND `target_is_linux()` is true, resolves the host's
   `git` binary via `resolve_host_git_dir()`.
3. Otherwise, leaves `host_git_dir` as `None`.

`resolve_host_git_dir()` walks `PATH` for an executable named `git`,
then falls back to `/usr/bin` and `/usr/local/bin`. It is gated to
Linux only; on Windows it returns `None` immediately.

The clean-room policy then prepends that directory to `PATH` while
still emitting the same isolation variables as the bundled branch:

```
GIT_CONFIG_NOSYSTEM=1
GIT_CONFIG_GLOBAL=<storage>/git/config/global.gitconfig
GIT_TERMINAL_PROMPT=0
HOME=<storage>/git/home
USERPROFILE=<storage>/git/home                # Windows only
XDG_CONFIG_HOME=<storage>/git/config          # Linux only — new
```

`LD_LIBRARY_PATH` is **not** set when using host git; the system
loader resolves the `.so` dependencies. `XDG_CONFIG_HOME` is new: it
prevents `~/.config/git/config` from leaking (which `GIT_CONFIG_NOSYSTEM`
does not cover).

When both bundled and host git are unavailable,
`bundled_package_manager_policy` returns `Err` with a remediation
message instead of letting pnpm fail with `git: not found`.

## What this means for Linux users

- `apt install git` (or distro equivalent) is now a soft requirement.
  pnpm-driven `git ls-remote` resolves through the host binary.
- Host `~/.gitconfig`, `/etc/gitconfig`, and `~/.config/git/config`
  cannot reach the child. The Box-owned empty `global.gitconfig` wins.
- Host `HOME` cannot leak; pnpm writes transient files under
  `<DSHBoxStorage>/git/home/`.
- When (if) DSH Box CI ships a Linux Git bundle, `runtime-lock.json`
  will get a `linux-x64`/`linux-arm64` git entry. The host fallback
  becomes dead code for those targets automatically.

## What this means for Windows users

Nothing changes. Windows still uses the bundled PortableGit only.
There is no host-git fallback on Windows by design — Windows users
without git should install Git for Windows manually rather than have
the daemon silently reach into the system.

## How to validate on Linux

1. Start with a clean machine that has Node (no pnpm), no bundled
   Git, and a host git installed:

   ```sh
   git --version
   # git version 2.43.0 (or whatever the distro ships)
   ```

2. Run a boxfile that triggers `ADD plugin github.com/...`:

   ```sh
   dshbox pull template github.com/deepseek-ai/deepseek-harness:<tag>
   dshbox build boxfile.dsh --name test
   ```

3. The build should succeed. Watch for:
   - `git ls-remote` resolving via the host binary (no `git: not found`).
   - `<DSHBoxStorage>/git/home/` being created (HOME redirect).
   - `<DSHBoxStorage>/git/config/global.gitconfig` being created
     (GIT_CONFIG_GLOBAL redirect).
   - The user's host `~/.gitconfig` sentinel (e.g. an `http.proxy`
     line) NOT being read by the child.

4. Optional regression: drop a sentinel
   `[http]\n\tproxy = sentinel` into `~/.gitconfig`, then `dshbox
   build` and confirm the pnpm child does not pick up that proxy.

## How to disable the fallback

The fallback is compile-gated to Linux and runtime-gated by
`target_is_linux()`. To opt out for a particular environment, install
git in a non-PATH location (e.g. `/opt/custom/bin/git`) and the
resolver will return `None` for the missing PATH lookup and the
`/usr/bin`/`/usr/local/bin` fallbacks. The daemon will then error with
"DSH Box requires git on this platform" rather than using a binary
the operator did not approve.

## What we do NOT need from the Linux team yet

- No CI changes — this PR does not add a Linux build target.
- No `runtime-lock.json` `linux-x64` entry — leave it absent; the
  fallback exists exactly so the daemon works without it.
- No changes to `box-runtime`'s Windows-only `cfg!(windows)` sites.

## What we WILL need later (out of scope)

If/when the team wants to publish a bundled Linux Git distribution to
match Windows, the CI work mirrors the PortableGit flow:

1. Build Git from upstream source in a glibc 2.28 (Ubuntu 18.04)
   container.
2. Bundle every non-system `.so` under `<runtime>/git/lib/`.
3. Publish a `.tar.zst` with a pinned SHA-256.
4. Add a `linux-x64` (and `linux-arm64`) entry to `runtime-lock.json`
   with `entry: "bin/git"`.
5. The fallback in this PR becomes a no-op for that target.

That work is tracked separately; the fallback in this PR is the
interim measure.

## References

- `docs/design/bundled-git-runtime.md` — full design doc, including
  the new "Linux host fallback" section added in this PR.
- `src-tauri/crates/box-runtime/src/bundled.rs` —
  `target_is_linux()`, `resolve_host_git_dir()`.
- `src-tauri/crates/box-runtime/src/process/env.rs` —
  `bundled_package_manager_policy`, `GitState` enum, the two new tests.
- `src-tauri/crates/dshboxd/src/toolchains.rs` — daemon-side wiring.