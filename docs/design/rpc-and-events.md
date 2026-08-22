# RPC and event design

## Authority

`dshboxd` is the only writer of runtime state. Tauri adapters, CLI commands,
and the React UI call the same loopback RPC methods and observe the same task
and resource events. A client must not reimplement a lifecycle step locally.

## Long-operation contract

Mutating methods return a persisted `TaskRecord` immediately. Workers emit
ordered `task:stage`, `task:log`, and terminal `task:finished` events; successful
publication also emits `resource:added` or `resource:updated`. Read operations
return a current `ResourceStateManager` snapshot.

The schema-10 lifecycle uses these user-visible stages:

```text
template pull:       cloning → installing dependencies → building frontend
                     → validating prepared base → publishing
plugin import:       preparing (if approved) → packing artifact → validating
                     → publishing
template build:      copying base → adding local plugin artifacts → validating
                     → materializing links → publishing
container create:    copying sealed template → writing container state
                     → publishing
container start:     allocating port → launching DSH host → waiting ready
```

`container create` must never emit `installing DSH dependencies` or `building
DSH frontend`. Those stages are evidence of the retired shared-runtime flow and
should be rejected in tests and UI fixtures.

## Resource payload requirements

Prepared-base and sealed-template responses include their id, digest, source
ref/commit, toolchain versions, size, validation time, and task/log links.
Container responses include source sealed-template id/digest and its local paths.
Plugin responses include artifact digest and provenance. Paths returned to UI are
diagnostic/display paths only; they never authorize a client to construct a
different process command.

## Failure and recovery

All publishable operations write only under `staging/<task-id>` until validation
succeeds. A failure reports its diagnostic log and leaves no resource event or
partially visible resource. Retrying creates a fresh staging tree. The daemon
does not repair a published base/template/container in place.

Host launch allocates a loopback port immediately before spawning the process
and uses bounded bind/readiness retries. Its recovery behavior must not invoke
pnpm install/build.

Full storage and scheduling rules are in
[prepared-template-runtime.md](../specs/prepared-template-runtime.md) and
[data-scheduler.md](../specs/data-scheduler.md).
