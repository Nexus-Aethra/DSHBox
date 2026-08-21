# DSH Box handoff snapshot

Updated: 2026-08-22

This snapshot supersedes the earlier metadata-only built-template handoff. The
project is moving to storage schema 10 and a physically materialized lifecycle.
The complete implementation plan is
[docs/specs/prepared-template-runtime.md](docs/specs/prepared-template-runtime.md).

## Bundled Git runtime (2026-08-22)

Windows ships Git-for-Windows PortableGit inside `<runtime>/git/` (pinned +
SHA-256-verified via `runtime-lock.json`; extraction needs developer-side 7-Zip,
which never ships in installers). The clean-room package-manager policy
prepends `<runtime>/git/cmd` to PATH and redirects HOME/GIT_CONFIG_GLOBAL/
GIT_TERMINAL_PROMPT/XDG_CONFIG_HOME under `<storage>/git/…`. On Linux the same
policy falls back to the host's git binary (PATH → /usr/bin → /usr/local/bin,
optionally `BoxConfig.git_path`) while applying the identical config isolation;
Windows never falls back. Design of record:
[docs/design/bundled-git-runtime.md](docs/design/bundled-git-runtime.md);
Linux-team handoff note:
[docs/notes/2026-08-21-linux-host-git-passthrough.md](docs/notes/2026-08-21-linux-host-git-passthrough.md).

Related hardening in the same window: pnpm install retries on transient
Windows file-lock failures ([UNKNOWN]/EBUSY/EPERM classifier, exponential
backoff), and `container_plugin_add` now runs the DSH CLI against the
container-local `harness/` copy instead of the removed legacy
`runtimes/<version>/source` layout.

### Defender exclusions are registered by the desktop shell (2026-08-22)

The retry classifier loses against Windows Defender on fresh installs: the
real-time scanner opens every freshly materialized `node_modules` file exactly
while pnpm reads it back for bin linking, so all four attempts fail with
`[UNKNOWN] unknown error, open '…package.json'`. The desktop app now asks once
via UAC (first launch, and again only when the runtime directory changes) to
exclude the install directory and the runtime data directory
(`src-tauri/src/desktop/app/defender.rs`; outcome persisted as
`BoxConfig.defender_exclusions_for`, declined prompts are remembered and never
re-shown). `scripts/register-defender-exclusion.ps1` remains the manual
fallback. Without the exclusion, container prepare fails deterministically —
treat `[UNKNOWN] UNKNOWN` prepare failures as an AV-scan race first.

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
