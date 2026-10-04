# @nexus-aethra/dsh-box-sandbox

Host-side bridge from a DSH session to the DSH Box daemon.

## Why it exists

Developing a DSH plugin *inside* DSH is a crash story: a bad edit takes
down the host that is running it. DSH Box stays outside that loop, so the
development sandbox is the one process the agent is allowed to break. This
plugin makes that sandbox reachable over a typed transport instead of by
shelling out to the `dshbox` CLI and scraping stdout.

## Wire contract

Verified against `src-tauri/crates/dshboxd/src/main.rs`. The details below
are the ones that are easy to get wrong:

- **`POST /rpc` is matched exactly.** `path != "//rpc"` is a 404, so the
  path must carry no query string.
- **The token travels in the JSON body**, not in an `Authorization`
  header. `box-client` also sends `Bearer`, but the daemon never reads it.
- **Params are flattened** into the top-level request object alongside
  `token` and `method` — there is no nested `params` field.
- **Replies**: `{ ok: true, result }` for sync methods,
  `{ ok: true, task }` for async ones, `{ ok: false, error }` otherwise.
  The daemon closes the connection after each frame.
- **`GET /events?token=...`** is the SSE stream; that endpoint takes the
  token from the query string.

## Discovery

The daemon binds a **dynamic** loopback port and mints a token per launch,
then publishes both atomically:

    <config-dir>/server/discovery.json

where `<config-dir>` is `DSHBOX_CONFIG_DIR` if set, otherwise `~/.dsh-box` —
the same precedence `box_foundation::config_path` applies on the Rust side.
The record is `{ token, pid, startedAt, port }` and is removed on shutdown.

Ports are never hardcoded. The record is re-read on every call, and a
transport failure is retried exactly once against a fresh read, so a daemon
restart (new port, new token) is transparent to callers.

## Usage

    import { getRpc, status } from '@nexus-aethra/dsh-box-sandbox'

    const pong = await getRpc().ping()
    const containers = await getRpc().call('list_containers')

    // Probe without throwing — for prompt surfaces and diagnostics.
    const s = await status()
    if (s.problem) console.log('dshbox unavailable:', s.problem)

`status()` never throws: it returns the discovery path, the endpoint when
reachable, and a `problem` string otherwise. A failed mount is never
fatal — a missing daemon is a normal first-run state, not a load error.

## Config

    - insert:
        - id: dsh-box-sandbox
          name: '@nexus-aethra/dsh-box-sandbox'
          config: {}

| Field | Default | Meaning |
|---|---|---|
| `configDir` | `''` | Empty means resolve like the daemon does. |
| `timeoutMs` | `30000` | Per-attempt request timeout. |

## Security

The discovery token is full control of the daemon: create and delete
containers, run builds, install plugins. Anything that can read the record
can drive dshbox. That is the intent here, but it is a deliberate trust
boundary — do not mount the host profile directory into an untrusted
container.

## Development

    pnpm install
    pnpm typecheck
    pnpm check:outputs   # static: schema vs what the daemon returns
    pnpm check:tools     # live: every tool called against a running Box
    pnpm build

`check:tools` needs a running daemon: it imports the built plugin with a stub
host context, calls every tool's `execute` for real, and validates each answer
against that tool's own output schema — the same check the host applies, at the
moment the answer comes back. It exists because a tool whose schema disagrees
with its result does not fail at build time; it fails when someone presses the
button, and the only way to see that is to press it.

`check:tools` is a local check and does not run in CI, which has no daemon.
The tag pipeline runs `check:outputs` instead, because that one needs nothing
but the repository.

    node smoke.mjs   # transport-level: ping, stale-port failure, recovery

`smoke.mjs` reads the real discovery record and checks the wire contract.

## Releasing

This package has its own version line, separate from the DSH Box app's — the
app was at 0.1.8 while this was at 0.1.0, and binding it to the app tag would
force it to 0.1.9 next with the versions between never existing.

So the trigger is its own tag, `dsh-box-sandbox-v<version>`. It does not start
with `v`, so `release.yml` (which matches `v*`) does not also fire and build
installers for a plugin push.

1. Bump `version` in `package.json`.
2. Run `pnpm check:tools` against a real Box. CI cannot — it has no daemon —
   so this is the only place the live check happens.
3. Commit, tag `dsh-box-sandbox-v<version>`, push the tag.

The workflow refuses a tag that disagrees with `package.json`, builds `dist/`
(which is gitignored, so it is never restored from the commit), runs the type
check and the schema reconciliation, shows the tarball contents, skips a
version npm already has, refuses to publish below what npm has, and publishes
with provenance.

Auth is npm trusted publishing over OIDC — there is no long-lived token to
leak. `secrets.NPM_TOKEN` is read as a fallback; leave it unset if the package
is configured for trusted publishing.
