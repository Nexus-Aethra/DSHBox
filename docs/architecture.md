# DSH Box — Architecture

> System architecture, component relationships, data flow, and communication patterns.

## Overview

DSH Box is a desktop launcher and lifecycle manager for DeepSeek Harness (DSH). It is a Tauri 2 shell (Rust) + React management UI + `dshboxd` HTTP sidecar daemon.

The architecture follows a **daemon-centric model**: all business logic runs in the background `dshboxd` process. The CLI and desktop UI are thin clients that communicate exclusively through `POST /rpc` and `GET /events`SSE — they never own storage, tasks, or container state.

```
┌─────────────────────────────────────────────────┐
│                    User                          │
├──────────────┬──────────────────┬────────────────┤
│   CLI        │  Desktop UI      │  Agent (curl)  │
│  (dshbox)    │  (Tauri + React) │  (debug)       │
└──────┬───────┴────────┬─────────┴───────┬────────┘
       │                │                  │
       │ POST /rpc      │ Tauri IPC        │ POST /rpc
       │ GET /events    │ (→ /rpc)         │ GET /events
       │                │ (← daemon://event)│
       ▼                ▼                  ▼
┌───────────────────────────────────────────────┐
│              dshboxd (HTTP sidecar)            │
│  dispatch.rs  events.rs  state.rs  host.rs    │
│  versions.rs  image.rs   lifecycle.rs          │
└───────────────────────────────────────────────┘
       │
       ▼
┌───────────────────────────────────────────────┐
│  box-* crates (framework-free)                 │
│  box-scheduler  box-runtime  box-foundation    │
│  box-template-core  box-dsh-versions           │
│  box-containers  box-extensions  box-image     │
│  box-state  box-data-scheduler  box-logger     │
│  box-api  box-client  box-toolchains           │
└───────────────────────────────────────────────┘
```

## Component layers

### 1. Daemon layer (`dshboxd`)

The sidecar HTTP server. Single binary, no framework (no hyper/tokio — pure std `TcpServer`). Listens on a dynamic loopback port, publishes discovery via `~/.dsh-box/server/discovery.json`.

**Key modules:**

| Module | Responsibility |
|--------|---------------|
| `main.rs` | HTTP server (`POST /rpc`, `GET /events`, `GET /ping`), `handle_http`, `handle_sse` |
| `dispatch.rs` | RPC dispatch table — 62+ methods, `HandlerResult::Sync/Async` discrimination |
| `events.rs` | `DaemonEvents` broadcast channel, `DaemonEvent` enum (10 variants) |
| `state.rs` | `DaemonState` (TaskManager, ResourceStateManager, ContainerManager, BoxPaths), `DaemonNotifier`, `BundledRuntime` |
| `host.rs` | `HostState` — managed child-process registry for DSH containers |
| `lifecycle.rs` | `start_dsh_container_inner`, `stop_dsh_container`, `rebuild_dsh_container_with_task` |
| `versions.rs` | `pull_template_with_cancel`, `uninstall_dsh_version_rpc`, `refresh_dsh_catalog` |
| `extensions.rs` | `import_into_repository`, `container_plugin_add`, `install_plugin_dependencies` |
| `image.rs` | `build_image_from_script`, `materialize_template_container`, `list_templates` |
| `bundles.rs` | Extension bundle import/export/install |
| `containers.rs` | Container creation, DSH host launch orchestration |
| `toolchains.rs` | `resolve_toolchain`, `run_logged`, `pnpm_policy` |

### 2. Crate layer (framework-free)

All crates under `src-tauri/crates/` are pure Rust — no Tauri, no hyper, no tokio. They are designed to be testable in isolation.

| Crate | Role |
|-------|------|
| `box-foundation` | Config, paths, JSON persistence, validation, `BoxPaths`, `BoxConfig`, `read_config()` |
| `box-scheduler` | `TaskManager`, `TaskRecord`, `TaskState` (8-state machine), `run_queued`, `TaskNotifier` trait |
| `box-runtime` | `ProcessSpec`, `NativeProcessRunner`, `ExecutionKind`, `shallow_clone_with_cancel` (libgit2) |
| `box-template-core` | `install_template`, `uninstall_template`, `write_root_prepared_marker` |
| `box-dsh-versions` | `pull_template`, `parse_template_ref`, `TemplateEntry`, `TemplateKind`, `classify_kind` |
| `box-containers` | `DshContainer`, `scan_containers`, `host_pid_path`, `pid_is_alive` |
| `box-extensions` | `RepositoryExtension`, `scan_repository`, `write_repository_index`, reference counting |
| `box-image` | `write_dshimage`, `read_dshimage`, `ImageManifest`, `parse_script` |
| `box-state` | `ResourceStateManager`, `ResourceSnapshot`, `apply_task_update` |
| `box-data-scheduler` | Soft-delete + dual-queue async hard-delete (fast/slow/permanent) |
| `box-logger` | `init(component, log_dir)` — daily rolling file + stderr, `RUST_LOG` control |
| `box-api` | IPC DTOs (`CreateTemplateContainerRequest`, `BuildImageRequest`, etc.) |
| `box-client` | `RpcClient`, `RpcResponse` — HTTP client for daemon communication |
| `box-toolchains` | Toolchain discovery and resolution |
| `box-dsh-context` | DSH context snapshot, patch YAML rendering |

### 3. Desktop layer (Tauri)

The desktop app is a Tauri 2 shell. It owns the window, tray, and the React frontend. It communicates with the daemon through `box-client` (same HTTP code as the CLI).

| Module | Responsibility |
|--------|---------------|
| `app.rs` | `run_inner` — Tauri builder setup, tray, startup, signal handler |
| `app/events.rs` | `spawn_event_subscriber` — SSE subscriber, bridges `/events` → `daemon://event` |
| `app/rpc.rs` | `connect()`, `call()` — thin wrappers over `box_client` |
| `app/tasks.rs` | `TauriNotifier`, `queue_task`, `refresh_global_state`, `emit_task_update` |
| `app/commands/` | Tauri IPC commands (config, containers, versions, toolchains, state) |

### 4. Frontend layer (React)

The React UI is intentionally minimal — no business logic, no state derivation. It emits IPC commands and listens to Tauri events.

| Hook | Purpose |
|------|---------|
| `useTasks` | Subscribes to `task://*` and `daemon://event`; maintains `tasks`, `taskLogs` |
| `useContainers` | Container list, CRUD operations |
| `useSettings` | Config, mirror, language settings |
| `useResources` | Resource state (templates, plugins, bundles) |

## Communication patterns

### RPC — dual-mode dispatch

Single entry point `POST /rpc`:

```
Request:  {"method":"<name>","param1":"val1","param2":"val2","token":"<token>"}

Response (sync):  {"ok":true, "result": <value>}
Response (async): {"ok":true, "task": <TaskRecord>, "eventsUrl": "/events"}
Response (error): {"ok":false, "error": "<message>"}
```

The daemon decides sync vs async at dispatch time via `HandlerResult`:

```rust
pub enum HandlerResult {
    Sync(Value),        // immediate response
    Async(TaskRecord),  // enqueued worker task
}
```

Sync handlers (`list_templates`, `ping`, `get_info`, `cancel_task`, etc.) return JSON directly. Async handlers (`pull_template`, `create_container_from_template`, `enqueue_container_start`, etc.) enqueue a task via `box-scheduler` and return the `TaskRecord` immediately.

### SSE event stream — `GET /events?token=...`

Long-lived TCP connection. On connect, the daemon sends a `snapshot` frame with the full current state. Subsequent frames are real-time events:

```
event: snapshot
data: {"tasks": [...], "resources": {...}}

event: TaskStage
data: {"id":"...","stage":"Installing","progress":45}

event: TaskLog
data: {"id":"...","log":"downloading..."}

event: TaskFinished
data: {"id":"...","status":"succeeded","error":null}

event: RollbackStarted / RollbackFinished / RollbackFailed
data: {"id":"...","status":"rolling_back"}

event: ResourceAdded / ResourceRemoved / ResourceUpdated
data: {"key":"runtime:v0.1.0","kind":"template"}

event: TagsFetched
data: {"tags":["v0.1.0","v0.2.0"]}
```

The desktop `events.rs` subscribes and re-emits each frame as a Tauri `daemon://event` payload. The CLI can debug with `curl -N`.

### Desktop event bridge

```
dshboxd /events (SSE)  ──TCP──>  events.rs (SSE subscriber)
                                       │
                                       │ app.emit("daemon://event", payload)
                                       ▼
                              useTasks.ts (React listener)
                                       │
                                       │ routes by event field
                                       ▼
                              TaskRecord reducer (tasks, taskLogs)
```

## Task state machine

8 states with validated transitions:

```
        ┌──────────┐
        │  Queued   │
        └─────┬─────┘
              │ try_start()
              ▼
        ┌──────────┐
        │  Running  │
        └──┬────┬──┘
           │    │
    finish()    │ error + rollback
           │    │
           ▼    ▼
   ┌──────────┐  ┌───────────────┐
   │ Succeeded│  │ RollingBack   │
   └──────────┘  └───────┬───────┘
                         │ finish_rollback()
                         ▼
                 ┌───────────────┐
                 │  RolledBack   │  ← RollbackFailed if finish_rollback errors
                 └───────────────┘

   Cancelled ←── cancel_requested before start
   Interrupted ←── interrupted during lock acquisition
   Failed ←── finish() with error (no rollback or rollback absent)
```

Transitions are validated by `TaskState::can_transition_to()`. The `TaskState` enum is serialised by `serde(rename_all = "lowercase")`.

## Resource ownership

The `box-scheduler` manages a `ResourceStateManager` map. Each task declares `resource_keys` (e.g. `["runtime:v0.1.0", "repository:extensions"]`). The scheduler enforces:

- A resource can only be held by one task at a time
- Queued tasks wait for their resources to become idle
- Finished tasks release their resources for the next queued task

The `box-data-scheduler` extends this with a soft-delete + dual-queue deletion model:

- **Fast queue**: immediate removal of index entries and metadata
- **Slow queue**: background fs-remove of large directory trees
- **Permanent queue**: periodic GC of unreferenced data

## Template system

Two template kinds:

| Kind | Source | Optimisation | `.dsh-prepared` |
|------|--------|-------------|-----------------|
| `Root` | `github.com/deepseek-ai/deepseek-harness:*` | Written on install | Skip pnpm install on DSH host startup |
| `Common` | Everything else | None | Not written |

`TemplateEntry` stores `kind` field (default `Common`). `classify_kind()` determines the kind from the ref string.

## Error handling philosophy

- **No silent fallback**: failed operations surface errors, never silently degrade
- **Diagnostic logs**: every failure path writes to `<runtime>/logs/<component>.log`
- **Recovery view**: the UI shows a recovery view when the daemon is unreachable
- **Task error propagation**: failed tasks carry `error: Option<String>` and `rollback_error: Option<String>`
- **Rollback**: when a task fails and a rollback closure is provided, the scheduler executes it while holding locks