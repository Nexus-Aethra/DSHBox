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
mod extensions;
mod host;
mod image;
mod lifecycle;
mod state;
#[cfg(test)]
mod test_support;
mod toolchains;
mod versions;

use box_server_core::{read_discovery, remove_discovery, write_discovery, ServerDiscovery};
use state::{initialize_bundled_plugins, initialize_bundled_runtime, DaemonState};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    time::Duration,
};

pub fn run() {
    if let Err(error) = initialize_bundled_runtime() {
        eprintln!("warning: bundled runtime unavailable: {error}");
    }
    if let Err(error) = initialize_bundled_plugins() {
        eprintln!("warning: bundled plugins unavailable: {error}");
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
            eprintln!("dshboxd: cannot load state: {error}");
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

    eprintln!(
        "dshboxd listening on 127.0.0.1:{} (pid {})",
        port,
        std::process::id()
    );

    for stream in listener.incoming() {
        let token = discovery.token.clone();
        let state = state.clone();
        std::thread::spawn(move || {
            if let Ok(stream) = stream {
                handle_http(stream, state, &token);
            }
        });
    }
    remove_discovery();
}

/// Parse an HTTP/1.1 request and dispatch it to `dispatch::dispatch`.
///
/// Reads the request line and headers line-by-line (terminated by a blank
/// line), then reads exactly `Content-Length` bytes for the body. This
/// avoids blocking on `read_to_string` which waits for the client to close
/// the connection — in HTTP/1.1 the client waits for a response before
/// closing.
fn handle_http(mut stream: TcpStream, state: Arc<DaemonState>, token: &str) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    // Request line
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let request_line = line.clone();
    line.clear();

    // Header lines until blank line
    let mut content_length: u64 = 0;
    loop {
        if reader.read_line(&mut line).is_err() {
            write_http_error(&mut stream, 400, "bad request");
            return;
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse::<u64>().unwrap_or(0);
        }
        line.clear();
    }

    // Body: read exactly Content-Length bytes
    let mut body_bytes = Vec::with_capacity(content_length as usize);
    let mut remaining = content_length as usize;
    while remaining > 0 {
        let buf = match reader.fill_buf() {
            Ok(n) if n.is_empty() => {
                if remaining > 0 {
                    write_http_error(&mut stream, 400, "bad request");
                    return;
                }
                break;
            }
            Ok(n) => n,
            Err(_) => {
                write_http_error(&mut stream, 400, "bad request");
                return;
            }
        };
        let to_read = std::cmp::min(buf.len(), remaining);
        body_bytes.extend_from_slice(&buf[..to_read]);
        let _ = reader.consume(to_read);
        remaining -= to_read;
    }
    let body = match std::str::from_utf8(&body_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            write_http_error(&mut stream, 400, "invalid utf-8");
            return;
        }
    };

    // Method must be POST
    if !request_line.starts_with("POST ") {
        write_http_error(&mut stream, 405, "method not allowed");
        return;
    }

    // Path must be /rpc
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let path = parts.get(1).unwrap_or(&"");
    if path.trim_end_matches('/').trim_end_matches('/') != "/rpc" {
        write_http_error(&mut stream, 404, "not found");
        return;
    }

    // Parse JSON body
    let request: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            write_http_error(&mut stream, 400, "invalid json");
            return;
        }
    };

    // Auth check
    if request["token"].as_str() != Some(token) {
        write_http_error(&mut stream, 401, "unauthorized");
        return;
    }

    let response_value = dispatch::dispatch(&state, &request);
    let response_str = serde_json::to_string(&response_value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"serialize error"}"#.to_string());
    write_http_success(&mut stream, &response_str);

    if dispatch::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(0);
    }
}

fn write_http_success(stream: &mut TcpStream, body: &str) {
    let _ = writeln!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.flush();
}

fn write_http_error(stream: &mut TcpStream, status: u16, reason: &str) {
    let body = serde_json::json!({"ok": false, "error": reason});
    let body_str = serde_json::to_string(&body).unwrap_or_else(|_| r#"{"ok":false}"#.to_string());
    let _ = writeln!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body_str.len(),
        body_str
    );
    let _ = stream.flush();
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
            eprintln!("dshboxd: removing stale discovery at {} (pid {}) — proceeding", addr, existing.pid);
            remove_discovery();
            None
        }
    }
}

fn main() {
    run();
}