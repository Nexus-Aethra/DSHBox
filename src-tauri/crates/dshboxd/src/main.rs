//! DSH Box daemon — owns the task queue, the bundled runtime, the running
//! container registry, and every business operation. CLI and GUI are thin
//! clients talking JSON-over-HTTP to this process (docker-style: `dockerd`
//! ↔ `docker`).
//!
//! Transport: TCP on `127.0.0.1:0` (OS-assigned ephemeral port), HTTP/1.1
//! over `std::net`. Zero external dependencies. Single POST route: `/rpc`.

mod bundles;
mod containers;
mod data;
mod dispatch;
mod events;
mod extensions;
mod host;
mod image;
mod lifecycle;
mod state;
#[cfg(test)]
mod test_support;
mod toolchains;
mod versions;

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
    let hosts = match state.containers.running.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let drained: Vec<(String, u32)> = hosts
        .iter()
        .filter_map(|(id, host)| host.child.id().map(|pid| (id.clone(), pid)))
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
        if let Some(pid) = host.child.id() {
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
        let query_token = path.split('?').nth(1).unwrap_or("")
            .split('&')
            .find_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                let key = kv.next()?;
                let val = kv.next()?;
                if key == "token" { Some(val.to_owned()) } else { None }
            });
        if query_token.as_deref() != Some(token) {
            let _ = write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        handle_sse(stream, &state);
        return;
    }

    // POST /rpc — JSON-RPC
    if method != "POST" || path != "/rpc" {
        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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
        let _ = write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };

    let Ok(request) = serde_json::from_str::<serde_json::Value>(&body) else {
        let _ = write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };

    if request["token"].as_str() != Some(token) {
        let _ = write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }

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

/// SSE event stream: subscribe to the daemon's event bus and write received
/// events as `text/event-stream` lines. The connection stays open until the
/// client disconnects.
fn handle_sse(mut stream: TcpStream, state: &DaemonState) {
    // Write SSE headers
    if writeln!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
    ).is_err() { return; }
    let _ = stream.flush();

    // Subscribe to events
    let (_sid, rx) = state.events.subscribe();

    // Write snapshot of current state
    let snapshot = serde_json::json!({
        "tasks": state.manager.list().unwrap_or_default(),
        "resources": {},
    });
    let _ = writeln!(stream, "event: snapshot\ndata: {}\n", snapshot);
    let _ = stream.flush();

    // Forward events as they arrive
    while let Ok(event) = rx.recv() {
        let name = event.event_name();
        let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
        let _ = writeln!(stream, "event: {name}\ndata: {json}\n");
        let _ = stream.flush();
    }
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
    match TcpStream::connect_timeout(&addr.parse::<std::net::SocketAddr>().ok()?, Duration::from_millis(250)) {
        Ok(_) => {
            // Port is reachable — old daemon is alive. Log and signal exit.
            eprintln!(
                "dshboxd: daemon already running on {} (pid {}) — exiting",
                addr,
                existing.pid
            );
            Some(existing)
        }
        Err(_) => {
            // Port unreachable — stale discovery from a crashed daemon.
            info!("removing stale discovery at {} (pid {}) — proceeding", addr, existing.pid);
            remove_discovery();
            None
        }
    }
}

fn main() {
    run();
}