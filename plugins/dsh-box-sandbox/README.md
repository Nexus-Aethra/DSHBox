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
    pnpm build
    node smoke.mjs   # 10 checks against a running DSH Box

`smoke.mjs` needs a live daemon: it reads the real discovery record, pings
it, forces a stale-port failure, and verifies recovery.
