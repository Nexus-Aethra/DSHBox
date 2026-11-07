//! DSH Box daemon — owns the task queue, the bundled runtime, the running
//! container registry, and every business operation. CLI and GUI are thin
//! clients talking JSON-over-HTTP to this process (docker-style: `dockerd`
//! ↔ `docker`).
//!
//! Transport: TCP on `127.0.0.1:0` (OS-assigned ephemeral port), HTTP/1.1
//! over `std::net`. Zero external dependencies. Single POST route: `/rpc`.

mod apply;
mod bundles;
mod containers;
mod data;
mod debug;
mod dispatch;
mod events;
mod extensions;
mod host;
mod lifecycle;
mod node_artifacts;
mod nodes;
mod nodes_rpc;
mod plugins;
mod resolve;
mod resources;
mod runtime;
mod sealed;
mod state;
#[cfg(test)]
mod test_support;
mod toolchains;
mod updates;
mod versions;
mod vm;
mod workspaces;

use crate::events::DaemonEvent;
use box_runtime::process;
use box_server_core::{read_discovery, remove_discovery, write_discovery, ServerDiscovery};
use state::{initialize_bundled_plugins, initialize_bundled_runtime, DaemonState};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    time::Duration,
};
use tracing::{error, info, warn};

pub fn run() {
    // Install SIGTERM/SIGINT/SIGHUP handlers so an externally signalled
    // shutdown still walks the graceful path. The shared atomic lets the
    // listener loop notice the request and call `graceful_shutdown`.
    process::install_signal_handlers();

    // Initialise the unified logger before any output. The log directory
    // is `<runtime>/logs/daemon/`; the file appender is daily-rolling.
    {
        let log_dir = box_logger::log_dir(
            box_logger::LogComponent::Daemon,
            box_foundation::read_config()
                .ok()
                .and_then(|c| c.runtime_directory)
                .as_deref(),
        );
        let _ = box_logger::init(box_logger::LogComponent::Daemon, &log_dir);
    }

    if let Err(error) = initialize_bundled_runtime() {
        warn!("bundled runtime unavailable: {error}");
    }
    if let Err(error) = initialize_bundled_plugins() {
        warn!("bundled plugins unavailable: {error}");
    }

    // Prevent multiple daemon instances: read the existing discovery file,
    // TCP-connect to the recorded port, and check whether the process
    // still responds. If it does, this is a second invocation against a
    // live daemon — exit immediately. If the port is dead (connection
    // refused / timeout), the discovery file is stale; remove it and
    // proceed with a fresh bind.
    if let Some(_existing) = ensure_single_instance() {
        return;
    }

    let state = match DaemonState::load() {
        Ok(state) => Arc::new(state),
        Err(error) => {
            error!("cannot load state: {error}");
            std::process::exit(1);
        }
    };
    // The plugin index mirrors what templates and containers actually resolved;
    // deriving it here means the plugin view is complete the first time it is
    // opened, without the user importing anything by hand.
    match box_foundation::read_config()
        .ok()
        .and_then(|config| config.runtime_directory)
    {
        Some(runtime) => match plugins::reconcile_plugin_index(std::path::Path::new(&runtime)) {
            Ok(0) => {}
            Ok(added) => tracing::info!(
                "plugin index: derived {added} plugin(s) from installed templates and containers"
            ),
            Err(error) => tracing::warn!("plugin index scan skipped: {error}"),
        },
        None => {}
    }

    // Scan persisted host.json records and reconcile each one against
    // the live process table. A record in `starting`/`ready`/`running`
    // whose PID no longer exists means the previous daemon died while
    // the container was up; we drop the stale record so a future
    // `container start` rebuilds it from scratch. A record whose PID
    // exists but returns EPERM (Unix) / `OpenProcess` access denied
    // (Windows) is flagged `orphaned` — the PID has been recycled by
    // an unrelated process and we cannot trust the URL anymore.
    crate::lifecycle::reconcile_orphan_containers();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind DSH Box daemon TCP");
    let port = listener.local_addr().expect("get local port").port();
    let discovery = write_discovery(port).expect("write server discovery");
    state
        .nodes
        .set_control_url(format!("http://127.0.0.1:{port}"));
    // Sweep stale helpers off the online list. Cooperative: the thread ends
    // with the daemon process, no join handle is kept.
    let sweep_nodes = state.nodes.clone();
    std::thread::spawn(move || loop {
        for _ in 0..60 {
            if process::shutdown_requested() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        sweep_nodes.sweep();
    });

    info!("listening on 127.0.0.1:{port} (pid {})", std::process::id());

    // Graceful shutdown: every managed host gets a TERM-by-pgroup, a
    // bounded wait, then a forced kill. Without this loop the OS would
    // hand every running container over to init as an orphan whenever the
    // daemon exits.
    let listener_state = state.clone();
    let listener_discovery = discovery.clone();
    std::thread::spawn(move || {
        while !process::shutdown_requested() {
            std::thread::sleep(Duration::from_millis(200));
        }
        graceful_shutdown(&listener_state);
        remove_discovery_with(&listener_discovery);
        std::process::exit(0);
    });

    for stream in listener.incoming() {
        let token = discovery.token.clone();
        let state = state.clone();
        std::thread::spawn(move || {
            if let Ok(stream) = stream {
                handle_http(stream, state, &token);
            }
        });
        if process::shutdown_requested() {
            break;
        }
    }
    graceful_shutdown(&state);
    remove_discovery();
}

fn remove_discovery_with(_discovery: &ServerDiscovery) {
    let _ = remove_discovery();
}

/// Stop every managed host gracefully. Each host gets a TERM-by-pgroup,
/// then a 5 s grace window, then a forced kill. Stale state records are
/// cleared so the next daemon start doesn't try to resume dead hosts.
fn graceful_shutdown(state: &DaemonState) {
    // Close out anything still running before the hosts go, and say why. A
    // task whose daemon disappears is not a task that failed, and the
    // difference is the whole diagnosis: the log used to stop mid-line and
    // the next start found a task that was neither running nor finished, which
    // is indistinguishable from a hang.
    match state
        .manager
        .interrupt_running("the DSH Box daemon shut down while this task was running")
    {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!(count = ids.len(), "interrupted in-flight tasks on shutdown");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "cannot mark in-flight tasks as interrupted"),
    }
    let hosts = match state.containers.running.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let drained: Vec<(String, u32)> = hosts
        .iter()
        .filter_map(|(id, host)| host.handle.id().map(|pid| (id.clone(), pid)))
        .collect();
    for (id, pid) in &drained {
        let _ = id;
        let _ = process::kill_tree_pid(*pid, None, false);
    }
    drop(hosts);
    std::thread::sleep(Duration::from_secs(5));
    let mut hosts = match state.containers.running.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    for host in hosts.values_mut() {
        if let Some(pid) = host.handle.id() {
            let _ = process::kill_tree_pid(pid, None, true);
        }
    }
    hosts.clear();
}

/// Parse an HTTP/1.1 request and dispatch it to `dispatch::dispatch`.
///
/// Reads the request line and headers line-by-line (terminated by a blank
/// line), then reads exactly `Content-Length` bytes for the body. This
/// avoids blocking on `read_to_string` which waits for the client to close
/// the connection — in HTTP/1.1 the client waits for a response before

/// Bind the server-owned node identity onto an RPC request.
///
/// `session_node` is the node the validated session belongs to, or `None` for
/// a main-token (or one-shot register) caller. The body's `__nodeId` is always
/// removed first: it is a reserved key, not a parameter, so a caller that can
/// supply it is exactly the caller that must not be trusted with it. Handlers
/// then see one value, from one source, and can reject a request with none.
///
/// Kept as a pure function so the gateway rule has a test that does not need a
/// socket: deleting the call site in `handle_http` is a one-line regression
/// that this test alone cannot catch, but deleting the drop inside here can.
fn bind_request_node(request: &mut serde_json::Value, session_node: Option<&str>) {
    if let Some(map) = request.as_object_mut() {
        map.remove("__nodeId");
        if let Some(node_id) = session_node {
            map.insert("__nodeId".to_owned(), serde_json::json!(node_id));
        }
    }
}

/// Read the request line and headers of an incoming HTTP request. Returns
/// the method and path on success, or `None` on parse error (the caller
/// should write an error response and return).
/// Handle a single HTTP request on `stream`. Routes:
///
/// - `GET /events?token=...` → SSE event stream (long-lived connection).
/// - `POST /rpc` → JSON-RPC handler (returns immediately).
fn handle_http(mut stream: TcpStream, state: Arc<DaemonState>, token: &str) {
    let mut reader = BufReader::new(&mut stream);

    // Read request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }

    // Read headers until blank line, track Content-Length
    let mut content_length: u64 = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse::<u64>().unwrap_or(0);
        }
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let method = parts.first().unwrap_or(&"").to_string();
    let path = parts.get(1).unwrap_or(&"").to_string();

    // SSE event stream
    if method == "GET" && path.starts_with("/events") {
        let query_token = path
            .split('?')
            .nth(1)
            .unwrap_or("")
            .split('&')
            .find_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                let key = kv.next()?;
                let val = kv.next()?;
                if key == "token" {
                    Some(val.to_owned())
                } else {
                    None
                }
            });
        let is_main = query_token.as_deref() == Some(token);
        // A helper's session token may subscribe too; it must carry the
        // current epoch so tokens from an older daemon cannot linger.
        let epoch = path
            .split('?')
            .nth(1)
            .unwrap_or("")
            .split('&')
            .find_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                (kv.next()? == "epoch").then(|| kv.next().unwrap_or("").parse::<u64>().ok()).flatten()
            })
            .unwrap_or(0);
        // Resolve the session to its node, not just to a boolean: the stream
        // is filtered per node, so the identity has to travel with the scope.
        let session_node = if is_main {
            None
        } else {
            state.nodes.session_node_for(query_token.as_deref(), epoch)
        };
        let scope = if is_main {
            SseScope::ControlPlane
        } else if let Some(node_id) = session_node {
            SseScope::Node {
                id: node_id,
                bearer: query_token.clone().unwrap_or_default(),
            }
        } else {
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            return;
        };
        handle_sse(stream, &state, &scope);
        return;
    }

    // POST /rpc — JSON-RPC
    if method != "POST" || path != "/rpc" {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }

    // Read body from the BufReader (which may have buffered data)
    let mut body_bytes = Vec::with_capacity(content_length as usize);
    let mut remaining = content_length as usize;
    while remaining > 0 {
        let buf = match reader.fill_buf() {
            Ok(n) if n.is_empty() => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let to_read = std::cmp::min(buf.len(), remaining);
        body_bytes.extend_from_slice(&buf[..to_read]);
        reader.consume(to_read);
        remaining -= to_read;
    }
    let Ok(body) = String::from_utf8(body_bytes) else {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let Ok(mut request) = serde_json::from_str::<serde_json::Value>(&body) else {
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let supplied = request["token"].as_str();
    let method = request["method"].as_str().unwrap_or("");
    let is_main = supplied == Some(token);
    // A session token is scoped to the node RPCs. It is never accepted for
    // control-plane methods, so a leaked helper credential cannot drive
    // containers, templates or tasks.
    let epoch = request["epoch"].as_u64().unwrap_or(0);
    // Resolve the session first (valid token + current epoch), THEN decide
    // whether the method is allowed. Filtering inside this block would turn
    // a valid-session/forbidden-method request into a 401 instead of 403.
    let session_node = if is_main {
        None
    } else {
        state.nodes.session_node_for(supplied, epoch)
    };
    // node_register is the one exception: it carries a one-shot enroll
    // grant instead of a session token, and the handler validates it.
    let is_register = method == "node_register"
        && request["enrollToken"].as_str().is_some_and(|v| !v.is_empty());
    let is_session = session_node.is_some();
    if !is_main && !is_session && !is_register {
        let _ = write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    if is_session
        && !crate::nodes::NodeManager::is_node_method(method)
        && method != "node_register"
    {
        let _ = write!(
            stream,
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    // `__nodeId` is server-owned. Drop whatever the caller sent, THEN bind it
    // from the validated session. A main-token caller has no session, so it has
    // no `__nodeId` at all and node handlers reject the request. Without the
    // unconditional drop a discovery-token holder could name any node in the
    // body and write an artifact row on its behalf (I3, design section 2.2).
    bind_request_node(&mut request, session_node.as_deref());

    let response_value = dispatch::dispatch(&state, &request);
    let response_str = serde_json::to_string(&response_value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"serialize error"}"#.to_string());
    let _ = writeln!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_str.len(),
        response_str
    );
    let _ = stream.flush();

    if dispatch::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(0);
    }
}

/// How often an open stream re-checks that its credential is still live. Short
/// enough that a revocation takes effect while the user is still watching,
/// long enough that an idle stream costs nothing worth measuring.
const REVOKE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// What an SSE subscriber is allowed to see. Two kinds exist: the control
/// plane (main token) sees everything, and a node helper sees its own node
/// and nothing else.
///
/// This is a type rather than a bool because the difference is not just
/// "more or less": a helper must be filtered *by node id*, and a bare
/// flag cannot express that. Passing the wrong thing then reads as a
/// compile error instead of a silent leak.
enum SseScope {
    ControlPlane,
    /// The node id *and* the bearer that opened the stream. The bearer matters:
    /// two grants can be minted for one node id, and the second `register`
    /// replaces the session. Keeping only the id would let the first helper's
    /// stream stay alive on the second helper's credential.
    Node { id: String, bearer: String },
}

impl SseScope {
    fn is_main(&self) -> bool {
        matches!(self, Self::ControlPlane)
    }

    fn node_id(&self) -> Option<&str> {
        match self {
            Self::ControlPlane => None,
            Self::Node { id, .. } => Some(id),
        }
    }

    /// Whether this subscriber may receive the event. A node helper gets
    /// node events for its own node only: `node_log` carries text another
    /// guest controls, so an unscoped feed lets one guest write into another
    /// guest's view.
    fn allows(&self, event: &DaemonEvent) -> bool {
        match self {
            Self::ControlPlane => true,
            Self::Node { id, .. } => {
                event.is_node_event() && event.node_id() == Some(id.as_str())
            }
        }
    }

    /// Re-check the credential behind an already-open stream.
    ///
    /// A session is validated once when the stream opens, and `node_deregister`
    /// removes it afterwards. Checking only at connect means revocation never
    /// revokes: a deregistered helper would keep receiving its node's events for
    /// as long as its socket stayed open, which is exactly the window a
    /// revoked credential must not have.
    fn still_valid(&self, state: &DaemonState) -> bool {
        match self {
            Self::ControlPlane => true,
            // Per credential, not per node: `has_session` would happily accept a
            // *different* helper that re-registered under the same node id.
            Self::Node { id, bearer } => state.nodes.session_still_matches(id, bearer),
        }
    }
}
/// SSE event stream: subscribe to the daemon's event bus and write received
/// events as `text/event-stream` lines. The connection stays open until the
/// client disconnects.
fn handle_sse(mut stream: TcpStream, state: &DaemonState, subscriber: &SseScope) {
    // Write SSE headers
    if writeln!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
    )
    .is_err()
    {
        return;
    }
    let _ = stream.flush();

    // Subscribe before building the snapshot, so an event that lands between
    // the two is delivered rather than dropped on the floor.
    let (sid, rx) = state.events.subscribe();

    // Snapshot: the control plane gets tasks, resources and nodes; a helper
    // gets its own node record only. A NodeRecord carries a control URL and a
    // helper version, and other nodes are not its business.
    let nodes = state.nodes.list().unwrap_or_default();
    let visible: Vec<_> = match subscriber.node_id() {
        Some(id) => nodes.into_iter().filter(|n| n.id == id).collect(),
        None => nodes,
    };
    let snapshot = if subscriber.is_main() {
        serde_json::json!({
            "tasks": state.manager.list().unwrap_or_default(),
            "resources": {},
            "nodes": visible,
        })
    } else {
        serde_json::json!({ "nodes": visible })
    };
    let _ = writeln!(stream, "event: snapshot\ndata: {}\n", snapshot);
    let _ = stream.flush();

    // Forward events as they arrive, filtered by scope. `allows` is what keeps
    // one guest from reading another node's log lines.
    //
    // `recv_timeout` rather than `recv`: a revocation (the node deregistered,
    // or the daemon restarted and sessions died with it) has to reach an
    // already-open stream. A blocking `recv` would only notice when the next
    // unrelated event happened to arrive, which for an idle node is never.
    loop {
        let event = match rx.recv_timeout(REVOKE_CHECK_INTERVAL) {
            Ok(event) => event,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if subscriber.still_valid(state) {
                    continue;
                }
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !subscriber.still_valid(state) {
            break;
        }
        if !subscriber.allows(&event) {
            continue;
        }
        let name = event.event_name();
        let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
        if writeln!(stream, "event: {name}\ndata: {json}\n").is_err() {
            break;
        }
        let _ = stream.flush();
    }
    // A client that vanished leaves a subscriber holding a live sender, which
    // `broadcast` prunes only when a send fails — and a send to a live receiver
    // never fails. Dropping it here is what actually frees the thread.
    state.events.unsubscribe(sid);
}

/// Prevent multiple daemon instances from competing on the same discovery file.
///
/// On startup, reads the previously written discovery.json. If a port is
/// recorded, TCP-connects to it. A successful connect means the previous
/// daemon is still alive and already serving — this invocation is a
/// duplicate and should exit immediately. A failed connect (refused /
/// timeout) means the previous daemon died ungracefully and left a stale
/// discovery file behind — remove it and proceed with a fresh bind.
///
/// Returns `Some(discovery)` when an older daemon is confirmed alive
/// (caller should exit). Returns `None` when no conflict exists (caller
/// should proceed to bind and write a new discovery).
fn ensure_single_instance() -> Option<ServerDiscovery> {
    let existing = match read_discovery() {
        Ok(Some(discovery)) => discovery,
        Ok(None) => return None,
        Err(_) => return None,
    };

    let addr = format!("127.0.0.1:{}", existing.port);
    match TcpStream::connect_timeout(
        &addr.parse::<std::net::SocketAddr>().ok()?,
        Duration::from_millis(250),
    ) {
        Ok(_) => {
            // Port is reachable — old daemon is alive. Log and signal exit.
            eprintln!(
                "dshboxd: daemon already running on {} (pid {}) — exiting",
                addr, existing.pid
            );
            Some(existing)
        }
        Err(_) => {
            // Port unreachable — stale discovery from a crashed daemon.
            info!(
                "removing stale discovery at {} (pid {}) — proceeding",
                addr, existing.pid
            );
            remove_discovery();
            None
        }
    }
}

fn main() {
    // Hidden subcommand: the libkrun VMM child process. The daemon spawns
    // itself so the blocking `krun_start_enter` lives in a killable child
    // rather than a daemon thread.
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--vmm-run" {
        #[cfg(target_os = "linux")]
        {
            std::process::exit(crate::vm::libkrun::vmm_main(&args[2..]));
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("--vmm-run is only available on Linux");
            std::process::exit(2);
        }
    }
    run();
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::state::DaemonState;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use box_state::ResourceStateManager;
    use serde_json::{json, Value};
    use std::io::{BufReader, Read, Write};
    use std::sync::{Mutex, RwLock};

    fn state() -> Arc<DaemonState> {
        let events = Arc::new(crate::events::DaemonEvents::new());
        let empty = BoxPaths {
            config: std::path::PathBuf::from("/nonexistent"),
            runtime: None,
        };
        Arc::new(DaemonState {
            manager: TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: std::path::PathBuf::from("/nonexistent"),
                runtime: None,
            }),
            containers: Arc::new(Default::default()),
            resources: ResourceStateManager::default(),
            events: events.clone(),
            browser: Mutex::new(crate::debug::Sessions::new()),
            nodes: Arc::new(crate::nodes::NodeManager::load(
                &empty,
                String::new(),
                events,
            )),
            artifacts: crate::node_artifacts::ArtifactIndex::memory(),
        })
    }

    /// Drive a request through the real HTTP gateway, not a handler. The
    /// server thread runs `handle_http` with `main` as its discovery token.
    fn rpc(state: Arc<DaemonState>, main: &str, body: Value) -> (u16, Value) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = main.to_owned();
        let server_state = state.clone();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_http(stream, server_state, &token);
        });
        let payload = body.to_string();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            client,
            "POST /rpc HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
        .unwrap();
        client.flush().unwrap();
        let mut raw = String::new();
        BufReader::new(client).read_to_string(&mut raw).unwrap();
        handle.join().unwrap();
        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("").trim())
            .unwrap_or(json!({}));
        (status, value)
    }

    fn report_body(node_id: &str) -> Value {
        json!({
            "method": "node_artifact_report",
            "__nodeId": node_id,
            "baseId": "base-9c1f00d4a1b2c3d4",
            "sourceCommit": "0d1f500",
            "platform": "linux-x86_64",
            "archive": "/var/dshbox/artifacts/base.tar",
            "sha256": format!("9f2c{}", "a".repeat(60)),
            "sizeBytes": 1024,
        })
    }

    /// The reserved key belongs to the server. A main-token caller cannot
    /// name a node in the body: with no session there is nothing to bind and
    /// the report is refused. This is the regression mutation B exposed.
    #[test]
    fn a_main_token_cannot_forge_a_node_identity_in_the_body() {
        let state = state();
        let main = "main-token";
        let (grant, id, _, _) = state
            .nodes
            .enroll("wsl2", "linux-x86_64", vec![], Some("node-a".to_owned()))
            .unwrap();
        state.nodes.register(&grant, None, None).unwrap();

        let mut body = report_body(&id);
        body["token"] = json!(main);
        let (status, reply) = rpc(state.clone(), main, body);
        assert_eq!(status, 200);
        assert_eq!(reply["ok"], false, "a forged identity must not write a row");
        assert!(
            state.artifacts.list(None, None).unwrap().is_empty(),
            "the index stays empty"
        );
    }

    /// A session caller's own forged `__nodeId` is dropped and replaced by
    /// the session's node, even when the body names another node entirely.
    #[test]
    fn a_session_callers_forged_node_identity_is_replaced() {
        let state = state();
        let main = "main-token";
        let (g1, a, _, _) = state
            .nodes
            .enroll("wsl2", "linux-x86_64", vec![], Some("node-a".to_owned()))
            .unwrap();
        let (_, session, epoch, _) = state.nodes.register(&g1, None, None).unwrap();
        let (g2, b, _, _) = state
            .nodes
            .enroll("wsl2", "linux-x86_64", vec![], Some("node-b".to_owned()))
            .unwrap();
        state.nodes.register(&g2, None, None).unwrap();

        let mut body = report_body(&b); // claims the OTHER node
        body["token"] = json!(session);
        body["epoch"] = json!(epoch);
        let (status, reply) = rpc(state.clone(), main, body);
        assert_eq!(status, 200);
        assert_eq!(reply["ok"], true, "{reply}");
        let rows = state.artifacts.list(None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node_id, a, "the session wins, not the body");
        assert_eq!(rows[0].id, format!("{a}|base-9c1f00d4a1b2c3d4"));
    }


    /// `node_register` is the one route with no session but body-supplied
    /// fields. Stripping the reserved key must not break it: the helper's
    /// platform has to reach the record, and the enroll grant still works.
    #[test]
    fn a_helper_can_still_register_over_the_gateway_with_its_platform() {
        let state = state();
        let main = "main-token";
        let (_, enroll) = rpc(
            state.clone(),
            main,
            json!({
                "method": "node_enroll",
                "token": main,
                "kind": "wsl2",
                "platform": "windows-x86_64", // the supply adapter's guess
                "nodeId": "node-a"
            }),
        );
        assert_eq!(enroll["ok"], true, "{enroll}");
        let grant = enroll["result"]["enrollToken"].as_str().unwrap().to_owned();

        let (status, reply) = rpc(
            state.clone(),
            main,
            json!({
                "method": "node_register",
                "enrollToken": grant,
                "platform": "linux-x86_64", // what the helper actually runs on
                "helperVersion": "0.1.10"
            }),
        );
        assert_eq!(status, 200);
        assert_eq!(reply["ok"], true, "{reply}");
        let node = state.nodes.get("node-a").unwrap().unwrap();
        assert_eq!(node.platform, "linux-x86_64", "the helper's platform wins");
    }

    /// The pure helper's rule: drop first, then bind. A unit test so removing
    /// the drop (the I3 bypass) fails without needing a socket.
    #[test]
    fn bind_request_node_drops_before_it_binds() {
        let mut forged = json!({ "__nodeId": "node-b", "method": "node_artifact_report" });
        bind_request_node(&mut forged, None);
        assert!(forged.get("__nodeId").is_none(), "no session, no identity");

        bind_request_node(&mut forged, Some("node-a"));
        assert_eq!(forged["__nodeId"], "node-a");

        let mut session_forged = json!({ "__nodeId": "node-b" });
        bind_request_node(&mut session_forged, Some("node-a"));
        assert_eq!(
            session_forged["__nodeId"], "node-a",
            "the forged value is gone"
        );
    }
}
