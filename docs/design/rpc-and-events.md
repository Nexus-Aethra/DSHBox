# RPC Dispatch + SSE Event Streaming — Design

> Status: Implemented (commits aace4bc → f65cfc0)

## 1. Motivation

DSH Box has three client surfaces: a CLI (`dshbox`), a desktop Tauri UI, and debug agents (curl). Before this design, each surface had a different code path:

- CLI called `box_template_core` directly
- Desktop had its own `TaskManager` + inline worker threads
- Debugging required reading daemon logs

The goal was a **single HTTP entry point** that all clients use, with the daemon deciding sync vs async, and a real-time event stream for progress.

## 2. Dual-mode RPC — `POST /rpc`

### 2.1 Wire format

```
Request:
  POST /rpc HTTP/1.1
  Authorization: Bearer <token>
  Content-Type: application/json

  {"method":"<name>", "param1":"val1", ..., "token":"<token>"}

Response (sync):
  HTTP/1.1 200 OK
  Content-Type: application/json

  {"ok":true, "result": <value>}

Response (async):
  HTTP/1.1 200 OK
  Content-Type: application/json

  {"ok":true, "task": <TaskRecord>, "eventsUrl": "/events"}

Response (error):
  HTTP/1.1 200 OK
  Content-Type: application/json

  {"ok":false, "error": "<message>"}
```

### 2.2 HandlerResult enum

The dispatch table uses a discriminated return type:

```rust
pub enum HandlerResult {
    Sync(Value),
    Async(TaskRecord),
}
```

Every handler returns `Result<HandlerResult, String>`. The `dispatch()` function in `dispatch.rs` wraps each match arm:

```rust
// Sync handler
Some("list_templates") => list_templates().map(|items| Sync(json!(items))),

// Async handler
Some("pull_template") => enqueue_pull_template(state, request),
// enqueue_pull_template returns Result<HandlerResult, String>
// with enqueue_task_worker(...).map(HandlerResult::Async)
```

The final response serialiser discriminates:

```rust
match result {
    Ok(HandlerResult::Sync(value)) => json!({"ok": true, "result": value}),
    Ok(HandlerResult::Async(task)) => json!({"ok": true, "task": task, "eventsUrl": "/events"}),
    Err(error) => json!({"ok": false, "error": error}),
}
```

### 2.3 Sync vs Async classification

| Category | Methods | Handler type |
|----------|---------|-------------|
| Read-only | `ping`, `get_info`, `list_*`, `container_url`, `template_info` | Sync |
| Read/write | `cancel_task`, `delete_task`, `shutdown`, `save_*`, `remove_*` | Sync |
| Long-running | `pull_template`, `enqueue_*`, `create_container_from_template` | Async |

The daemon decides — the client never specifies sync/async.

### 2.4 RpcClient (box-client)

The `RpcClient` struct in `box-client` handles the raw TCP connection and response parsing:

```rust
pub struct RpcResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub task: Option<Value>,      // #[serde(default)]
    pub error: Option<String>,    // #[serde(default)]
    pub events_url: Option<String>, // #[serde(default)]
}
```

`call()` returns `result.or(task).unwrap_or(Value::Null)` — transparently handles both response shapes.

`enqueue()` is a type-safe wrapper that returns `Err` if the daemon replied without a `task` field.

## 3. SSE Event Stream — `GET /events?token=...`

### 3.1 Connection lifecycle

```
Client                         Daemon
  │                               │
  │── GET /events?token=xxx ────→│
  │                               │── verify token
  │←── HTTP/1.1 200 OK ──────────│
  │←── SSE headers ──────────────│
  │                               │── subscribe to DaemonEvents channel
  │←── event: snapshot ──────────│  (full current state)
  │←── data: {"tasks":[...]} ────│
  │                               │
  │  ... real-time events ...     │
  │                               │
  │←── event: TaskStage ─────────│
  │←── data: {"id":"...",...} ───│
  │                               │
  │  ... client disconnects ...   │
  │                               │── unsubscribe (rx dropped)
```

### 3.2 DaemonEvent enum

```rust
pub enum DaemonEvent {
    TaskStage { task_id, stage, progress },
    TaskLog { task_id, message },
    TaskFinished { task_id, status, error },
    RollbackStarted { task_id, stage, progress },
    RollbackFinished { task_id, status, error },
    RollbackFailed { task_id, error },
    ResourceAdded { key, kind },
    ResourceRemoved { key, kind },
    ResourceUpdated { key, kind },
    TagsFetched(Vec<String>),
}
```

### 3.3 Broadcast mechanism

The `DaemonEvents` struct wraps a `std::sync::mpsc::broadcast` channel (custom implementation in `events.rs`):

```rust
pub(crate) struct DaemonEvents {
    subscribers: Arc<Mutex<Vec<mpsc::Sender<DaemonEvent>>>>,
}

impl DaemonEvents {
    pub fn subscribe(&self) -> (usize, mpsc::Receiver<DaemonEvent>);
    pub fn unsubscribe(&self, id: usize);
    pub fn broadcast(&self, event: DaemonEvent);
}
```

The `DaemonNotifier` trait connects the scheduler to the event bus:

```rust
pub trait TaskNotifier: Send + Sync {
    fn stage(&self, task_id: &str, stage: &str, progress: u8);
    fn log(&self, task_id: &str, message: &str);
    fn finished(&self, task_id: &str, status: &str, error: Option<&str>);
}
```

`DaemonNotifier` implements `finished()` to broadcast `TaskFinished` on the event bus. The `RecordingNotifier` (used in tests) has default no-op implementations.

### 3.4 Desktop bridge (events.rs)

The desktop subscriber is pure std — no hyper, no reqwest, no tokio:

```rust
pub(crate) fn spawn_event_subscriber(app: AppHandle) {
    thread::spawn(move || {
        loop {
            match subscribe_once(&app) {
                Ok(()) => { backoff = 250ms; }
                Err(error) => { backoff = min(backoff * 2, 5s); }
            }
            thread::sleep(backoff);
        }
    });
}
```

The subscriber:
1. Reads `discovery.json` for token + port
2. Opens `GET /events?token=...` with `set_nodelay(true)`
3. Hand-parses SSE frames (`event:` / `data:` lines)
4. Forwards each as `app.emit("daemon://event", {"event": <name>, "payload": <json>})`

Reconnection: exponential backoff (250ms → 5s cap). Snapshot frame resets the full state, so missed events during a disconnect are safe.

## 4. Task state machine

### 4.1 States

```rust
pub enum TaskState {
    Queued,       // waiting for resource locks
    Running,      // worker executing
    Succeeded,    // finished with Ok(())
    Failed,       // finished with Err(...)
    Cancelled,    // cancelled before execution started
    Interrupted,  // interrupted during lock acquisition
    RollingBack,  // running rollback closure
    RolledBack,   // rollback completed
    // RollbackFailed is not a state — finish_rollback errors leave the
    // task in Failed state with rollback_error populated
}
```

### 4.2 Validated transitions

```rust
fn valid_transitions(&self) -> &'static [TaskState] {
    match self {
        Self::Queued      => &[Self::Running, Self::Succeeded, Self::Failed, Self::Cancelled, Self::Interrupted],
        Self::Running     => &[Self::Succeeded, Self::Failed, Self::RollingBack],
        Self::RollingBack => &[Self::RolledBack, Self::Failed],
        _                 => &[],
    }
}
```

`can_transition_to(target)` checks `target` is in the valid set. Every `TaskManager` method that changes state calls `transition_to()` before updating the status string.

### 4.3 Rollback

```rust
pub fn run_queued(
    manager: &TaskManager,
    paths: &BoxPaths,
    notifier: Arc<dyn TaskNotifier>,
    task_id: &str,
    work: impl FnOnce(&TaskContext) -> BoxResult<()>,
    rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>>,
)
```

When `work` returns `Err` and a `rollback` closure is provided:
1. `manager.start_rollback()` marks the task as `RollingBack` (keeps locks)
2. `rollback_fn(&context)` executes the rollback
3. `manager.finish_rollback()` marks as `RolledBack` (releases locks)

If `finish_rollback` fails, the task stays in `Failed` state with `rollback_error` set.

## 5. Template system

### 5.1 TemplateKind

```rust
pub enum TemplateKind {
    #[serde(default)]
    Common,  // user-provided templates, skill imports
    Root,    // deepseek-ai/deepseek-harness family
}
```

`classify_kind(ref_value)` returns `Root` for refs starting with `github.com/deepseek-ai/deepseek-harness`, `Common` for everything else.

### 5.2 Root optimisation

Root templates get a `.dsh-prepared` marker written to the version directory:

```json
{
    "version": "dsh-v0.1.0-rc.7",
    "harnessTag": "dsh-v0.1.0-rc.7",
    "preparedAt": 1787159062,
    "hasNodeModules": false
}
```

The DSH host startup reads this marker and skips `pnpm install` when present. This reduces container startup from ~30s to ~2s for already-pulled templates.

### 5.3 install_template flow

```
install_template(runtime, ref_value, cancelled)
  │
  ├─ 1. parse_template_ref(ref_value) → parsed
  ├─ 2. classify_kind(ref_value) → kind
  ├─ 3. pull_template(runtime, ref_value, cancelled)
  │      └─ shallow_clone_with_cancel(github_url, destination, revision, cancelled)
  │      └─ write_pulled_base_template (index entry + .dsh script)
  │      └─ return parsed.version
  ├─ 4. read_template_index → find entry
  ├─ 5. if kind == Root:
  │      └─ write_root_prepared_marker(version_dir, version, tag)
  └─ 6. data-scheduler: register resource
```

## 6. Process execution (box-runtime)

### 6.1 ProcessSpec

```rust
pub struct ProcessSpec {
    executable: PathBuf,
    arguments: Vec<String>,
    policy: EnvironmentPolicy,
    kind: ExecutionKind,
    new_process_group: bool,
}
```

`ProcessSpec::new(executable)` creates a spec with default values. Builder methods:
- `.arg(val)` / `.args(values)` — add arguments
- `.policy(policy)` — set environment policy
- `.kind(ExecutionKind::Logged)` — capture output
- `.new_process_group(bool)` — Linux process group

### 6.2 ExecutionKind

```rust
pub enum ExecutionKind {
    Logged,      // Capture stdout/stderr (for toolchain commands)
    Interactive, // Hand off to child (for DSH host)
}
```

### 6.3 resolve_toolchain

For pnpm/npm, `resolve_toolchain` returns the bundled Node executable as `path` and the script path as `arguments`:

```rust
"pnpm" => (
    runtime.node.clone(),                          // path = node.exe
    vec![runtime.pnpm.to_string_lossy().into_owned()],  // arguments = [pnpm.mjs]
),
```

The `ProcessSpec` callers must prepend `.args(&pnpm.arguments)` to construct the correct command: `node.exe pnpm.mjs --dir ...`.

## 7. box-client HTTP handling

The `RpcClient` uses raw TCP (`std::net::TcpStream`) — no `reqwest` or `hyper`. This keeps the daemon's dependency tree minimal and avoids startup time overhead.

```rust
fn exchange(&self, request: Value) -> Result<RpcResponse, String> {
    // 1. Connect to 127.0.0.1:<port>
    // 2. Write HTTP POST request with JSON body
    // 3. Read entire response into string
    // 4. Find \r\n\r\n header boundary
    // 5. Parse JSON body as RpcResponse
}
```

The `RpcClient` is instantiated once per `connect()` call. The CLI and desktop share the same `box-client` crate — no code duplication.

## 8. CLI integration

The CLI `rpc.rs` wraps `box_client`:

```rust
pub(crate) fn call(client, method, params) -> Result<Value, String>  // sync
pub(crate) fn enqueue(client, method, params) -> Result<TaskRecord, String>  // async
pub(crate) fn run_task(client, method, params) -> Result<(), String>  // enqueue + poll
pub(crate) fn wait_task(client, task_id) -> Result<(), String>  // poll task_status + print progress
```

The `run` command (for `dshbox run <template>`) is a three-step process:
1. `enqueue("create_container_from_template", ...)` — start the task
2. `wait_task(client, &task.id)` — block until completion, streaming progress
3. `call("list_containers", ...)` — find the new container and print its URL

## 9. Key invariants

1. **Single HTTP entry point**: All clients (CLI, desktop, curl) go through `POST /rpc`.
2. **Daemon decides sync/async**: The client never specifies `"async": true` — the dispatch table handles it.
3. **SSE is the only real-time channel**: No polling, no long-polling, no WebSocket. `curl -N` is the debug tool.
4. **No hyper/tokio in daemon**: Pure `std::net::TcpServer` + `std::thread`. The desktop SSE subscriber is also std-only.
5. **CLI = desktop = agent**: All three use the same `box-client` crate — drift between surfaces is impossible.
6. **Task state is the source of truth**: `TaskManager` persists state to `<runtime>/logs/tasks/`. The daemon's `TaskManager` is the single writer.
7. **Rollback runs under lock**: The rollback closure executes while the task's resource locks are still held, guaranteeing exclusive access.