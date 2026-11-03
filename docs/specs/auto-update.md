# Automatic update discovery and install

Status: implemented
Owner: DSH Box desktop + `dshboxd`

DSH Box ships on three platforms, is installed by a per-machine installer, and
is commonly left running for weeks. Until this feature an update meant "notice
that a new tag exists on GitHub and go download it by hand". This document is
the contract between the release pipeline, the daemon, the desktop shell and
the UI, so the four cannot drift.

## Two tiers of discovery

Checking for an update is cheap or it is not, depending on what the caller
needs to do with the answer:

| Tier | Request | Cost | Used for |
|------|---------|------|----------|
| Quick | `GET <mirror>/https://api.github.com/repos/Nexus-Aethra/DSHBox/releases/latest` | one JSON, ~2 kB | "is there something newer" — the badge |
| Full | the `latest.json` release asset | one JSON, ~2 kB | downloading, verifying, installing |

The **quick** tier answers a badge and nothing else. It cannot download
anything, because a release tag says nothing about the platform asset a given
machine needs and nothing about its bytes.

The **full** tier is a `latest.json` attached to every release. It names, per
platform, the installer asset, its size and its SHA-256. A download without a
digest is not a download this app will run: the manifest is what makes
"the bytes I fetched are the bytes CI built" checkable offline, after the
transfer, from a value the user can read.

When a release predates `latest.json` (v0.1.0 through v0.1.9), the full tier
reports `reason: "no-manifest"` and the UI offers the release page instead of
an install button. That is a real limitation of stale release data, not a
silent fallback to something unverified.

### `latest.json` (schema 1)

```json
{
  "schema": 1,
  "version": "0.2.0",
  "releasedAt": "2026-11-02T00:00:00Z",
  "notes": "one short paragraph",
  "notesUrl": "https://github.com/Nexus-Aethra/DSHBox/releases/tag/v0.2.0",
  "minimumVersion": "0.2.0",
  "platforms": {
    "win-x64": [
      {
        "url": "https://github.com/Nexus-Aethra/DSHBox/releases/download/v0.2.0/dshbox_0.2.0_x64_en-US.msi",
        "sha256": "…",
        "size": 167211655,
        "installer": "msi",
        "installMode": "auto"
      }
    ],
    "linux-x64": [
      { "url": "…/dshbox_0.2.0_amd64.deb", "sha256": "…", "size": 75334652, "installer": "deb", "installMode": "manual" },
      { "url": "…/dshbox-0.2.0-1.x86_64.rpm", "sha256": "…", "size": 75688208, "installer": "rpm", "installMode": "manual" }
    ],
    "macos-arm64": [
      { "url": "…/dshbox_0.2.0_aarch64.dmg", "sha256": "…", "size": 67103177, "installer": "dmg", "installMode": "auto" }
    ]
  }
}
```

Rules:

- `schema` is checked, not guessed. An unknown schema is a hard error; a
  future manifest cannot be half-understood.
- `minimumVersion` is the oldest build that may take this update in place.
  Below it the reason is `below-minimum` and the update is manual: an
  installer that assumes a layout this build does not have is not something to
  run unattended.
- Platform keys are `bundled_target()` names (`win-x64`, `linux-x64`,
  `macos-arm64`), identical to the resource directory names, so the key, the
  sidecar path and the installer all name the same machine the same way.
- A platform's value is an **array**, because Linux publishes both a `.deb`
  and a `.rpm` for one machine and the manifest must not pretend one of them
  is not there. The first entry is the preference; a platform that cannot use
  it falls to the next. One platform with one installer is an array of one,
  not a special case that the reader has to branch on.
- A `url` is stored **unrewritten**. The GitHub mirror is applied at request
  time (`mirror_url`), so a user who sets a mirror gets the mirror for a
  download from a manifest that was published before they set it.
- `installMode: "manual"` means this platform's installer needs a person; the
  box downloads and verifies the file, then names the command and the path.

## Policy

`BoxConfig.auto_update` is one of four strings, persisted in `config.json`
next to `githubMirror` because it is a machine-local preference the Settings
pane owns:

| Policy | Checks | Downloads | Installs |
|--------|--------|-----------|----------|
| `off` | never (the UI can still ask once) | no | no |
| `check` | yes, background | no | no |
| `download` | yes | yes, automatically | on user action |
| `auto` | yes | yes, automatically | on quit / restart |

The default is `check`. Silently replacing a binary is the kind of thing that
has to be asked for; detecting is not.

`BoxConfig.skipped_update_version` records "not this one": while it equals the
manifest version, the available update is reported as skipped and the badge
stays quiet. It clears itself when a newer version appears, and `clear_update_skip`
clears it early.

## Background check throttle

`check_interval_seconds` is 86400. `get_update_state` never blocks on the
network: if the last successful check is older than the interval and the policy
is not `off`, it answers from the recorded state and spawns one detached
thread that refreshes it and broadcasts `update_state` on the SSE bus. A
failed check records `lastError` and does **not** retry sooner than the
interval, so an offline machine does not hammer GitHub.

`check_update` is the explicit path: it always goes to the network and returns
the fresh state inline (20s request timeout, `.no_proxy()` like every other
loopback-adjacent client is careful about, mirror applied).

## Applying it

Downloading is a scheduler task (`update-download`), so it has a log, progress
and cancellation like every other long operation. The bytes land in
`<runtime>/updates/<version>/<filename>.part`, are hashed while streaming,
checked against the manifest, and only then renamed into place. A failure
leaves no file behind; a re-download of an already `ready` file is a no-op.

Installing ends the running processes, so it is the desktop's job, not the
daemon's — the daemon is one of the binaries being replaced and cannot be the
one to swap itself.

The desktop writes a detached **helper** that outlives it:

- **Windows (msi)**: wait for the launcher PID to exit, stop `dshboxd`
  (holding `dshboxd.exe` open would fail the install), run
  `msiexec /i <msi> /qn /norestart`, then relaunch the launcher from the same
  directory.
- **macOS (dmg)**: wait for exit, `hdiutil attach`, replace the `.app` beside
  the running one, detach, `open` it again.
- **Linux (deb/rpm)**: `installMode: "manual"`. The box verifies the file, and
  the UI shows the path and `sudo apt install <path>` — installing a package
  needs root, and a hidden `pkexec` from a GUI is not a thing to do on
  someone's behalf.

The helper script text is generated by a pure function and unit-tested; the
desktop only stages it and forks it. `install_update` therefore answers
"scheduled" before the process dies, and the UI's job is to say what is about
to happen, including that running containers stop.

## Wire surface

Daemon RPCs: `get_update_state`, `check_update`, `enqueue_update_download`,
`discard_update_download`, `skip_update_version`, `clear_update_skip`.

Desktop commands add the ones that end a process or open a folder:
`install_update`, `open_update_folder`, and `save_update_policy` (the config
write the Settings pane needs).

`get_update_state` answers one flat object so the pane is a single round trip:

```ts
type UpdateState = {
  currentVersion: string
  buildStamp: string
  policy: 'off' | 'check' | 'download' | 'auto'
  skippedVersion: string | null
  checkedAt: number | null
  lastError: string | null
  checkIntervalSeconds: number
  available: UpdateRelease | null   // newest release, whether or not newer
  download: UpdateDownload | null
}

type UpdateRelease = {
  version: string
  releasedAt: string | null
  notes: string | null
  notesUrl: string | null
  minimumVersion: string | null
  asset: UpdateAsset | null
  /** True when this release cannot be installed from inside the app. */
  manual: boolean
  /** 'no-manifest' | 'no-asset' | 'below-minimum' | 'manual-installer' */
  manualReason: string | null
}

type UpdateAsset = { platform, url, sha256, size, installer, installMode }

type UpdateDownload = {
  version: string
  state: 'downloading' | 'verifying' | 'ready' | 'failed'
  bytes: number
  totalBytes: number
  path: string | null
  error: string | null
}
```

`available` is always the newest release the box knows about — including one
that is not newer — so the pane can say "up to date" without a second request
and so a *downgrade* is never offered as an update.

## The version this compares against

`dshboxd` is an internal crate and keeps its own `0.1.0`; the version a user
sees lives in `src-tauri/Cargo.toml`. The updater must compare releases against
the second one, so `build.rs` embeds it as `DSHBOX_APP_VERSION` and the daemon
reports that from `get_info` as well.

Using `CARGO_PKG_VERSION` here is a silent, total failure: a `0.1.9` install
answered `0.1.0`, so `is_newer` called every published release newer and the
pane offered an "update" to the version already installed. There is a
regression test for exactly this, and it asserts the two versions *differ* --
if they ever coincide the test can no longer tell the mistake apart from the
correct wiring.

## What each policy actually does

A policy whose description outruns its behaviour is worse than no policy,
so the background pass acts on what it found rather than only recording it:
under `download` and `auto` a new release is fetched automatically (skipped
versions excepted, and an already-verified file is never re-fetched), while
`check` stops at detection. `auto` differs from `download` only in what
happens at quit.

The download runs on the queue like any other task, because the background
thread has no daemon state to borrow: it enqueues against the manager and the
paths it was given, and detaches a worker. That is why an automatic download
still shows up in the task panel with a log and a cancel button.
## Safety rails, stated as rules

1. Nothing is executed that was not hashed. No digest, no install.
2. A schema this build does not know is an error.
3. A version below `minimumVersion` never installs in place.
4. Never compare versions as strings: `0.1.10` > `0.1.9`, and a pre-release
   is older than its release. Comparison is a tested function, not a `sort`.
5. A failed check is reported, never papered over with the previous answer's
   timestamp.
6. The mirror is applied when the request is made, not when the manifest was
   written.