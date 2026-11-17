//! Thin HTTP client for the `dshboxd` daemon.
//!
//! Used by the `dshbox` CLI (and, in a later phase, the desktop app) so
//! every client talks to the daemon instead of running business logic in
//! its own process. `connect()` reads the discovery file written by the
//! daemon and, when the daemon is not running, attempts to spawn it from
//! `PATH` and waits for it to come up.

use box_server_core::{read_discovery, ServerDiscovery};
use serde_json::Value;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long one RPC may take before the client gives up.
///
/// The daemon is a local process and its synchronous methods answer in
/// milliseconds; a call that outlives this is a daemon that is stuck, and
/// waiting forever would hang whoever asked — a Tauri command thread, the CLI,
/// or the desktop's liveness probe. The desktop runs its commands off the main
/// thread, so this is a bound on how long an action can stay pending, not a
/// freeze.
const RPC_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Writing a loopback request is instant unless the socket is half-open.
const RPC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The liveness probe is polled (the desktop's startup gate every 800ms), so it
/// answers "not yet" rather than blocking the poll for a minute.
const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// Response frame every daemon method returns. The daemon now produces
/// either a `result` (sync) or a `task` (async) field, plus an
/// `eventsUrl` pointer when the call enqueued a worker; we accept both
/// shapes so legacy callers that read `result` keep working and async
/// callers can pick up the task record through the same struct.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcResponse {
    pub ok: bool,
    pub result: Option<Value>,
    #[serde(default)]
    pub task: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub events_url: Option<String>,
}

/// A connected client bound to one daemon discovery.
#[derive(Clone)]
pub struct RpcClient {
    port: u16,
    token: String,
}

impl RpcClient {
    /// Build a client from an existing discovery record.
    pub fn from_discovery(discovery: &ServerDiscovery) -> Self {
        Self {
            port: discovery.port,
            token: discovery.token.clone(),
        }
    }

    /// Bearer token the daemon issued for this session. Used by callers
    /// that need to open a second connection (for example the SSE stream
    /// in the desktop event subscriber) without re-reading discovery.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Loopback port the daemon is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Locate the daemon, spawning it from `PATH` when it is not running.
    ///
    /// Reads `discovery.json`; if the endpoint is not reachable it tries
    /// `spawn_daemon()` and polls for up to 3 seconds for the discovery
    /// file to be replaced by the freshly-started daemon.
    pub fn connect() -> Result<Self, String> {
        if let Ok(Some(discovery)) = read_discovery() {
            let client = Self::from_discovery(&discovery);
            if client.ping().is_ok() {
                return Ok(client);
            }
        }
        Self::spawn_daemon()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Ok(Some(discovery)) = read_discovery() {
                let client = Self::from_discovery(&discovery);
                if client.ping().is_ok() {
                    return Ok(client);
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "failed to reach dshboxd; start it with: dshboxd &"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    }

    /// The platform directory name the installer lays the sidecar out under.
    ///
    /// Shared with the desktop side so both agree on where a bundled daemon
    /// lives; a second spelling here is how a launcher ends up looking for a
    /// file the installer never wrote.
    pub fn bundled_target() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "linux-x64",
            ("linux", "aarch64") => "linux-arm64",
            ("windows", "x86_64") => "win-x64",
            ("windows", "aarch64") => "win-arm64",
            ("macos", "x86_64") => "macos-x64",
            ("macos", "aarch64") => "macos-arm64",
            _ => "unsupported",
        }
    }

    /// Where the sidecar sits next to an installed launcher.
    ///
    /// The installer writes the daemon to `<install>/server/<target>/dshboxd`
    /// and puts only `<install>` on `PATH`, so `Command::new("dshboxd")` cannot
    /// find it. That is not a corner case, it is every installed copy: the CLI
    /// reports "cannot start dshboxd" while the daemon is sitting beside it.
    pub fn bundled_server_path() -> Option<std::path::PathBuf> {
        let executable = if cfg!(windows) { "dshboxd.exe" } else { "dshboxd" };
        std::env::current_exe()
            .ok()?
            .parent()
            .map(|directory| {
                directory
                    .join("server")
                    .join(Self::bundled_target())
                    .join(executable)
            })
    }

    /// Start the daemon: from `PATH` first, then from the bundled sidecar.
    ///
    /// `PATH` stays first because it is how a developer runs the tree they are
    /// editing. The bundled path is the fallback that makes an installed CLI
    /// work at all, and it is tried rather than preferred so a developer's own
    /// build still wins.
    ///
    /// Both attempts are named on failure: reporting only the `PATH` one sends
    /// the reader to fix their shell profile when the real problem is that the
    /// sidecar beside the binary is missing.
    pub fn spawn_daemon() -> Result<(), String> {
        let path_error = match Self::spawn_named("dshboxd") {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(bundled) = Self::bundled_server_path() else {
            return Err(format!("cannot start dshboxd: {path_error}"));
        };
        Self::spawn_named(&bundled.to_string_lossy()).map_err(|bundled_error| {
            format!("cannot start dshboxd: not on PATH ({path_error}); beside the binary: {bundled_error} at {}", bundled.display())
        })
    }

    /// Spawn one daemon by name or path, applying the platform's spawn rules.
    fn spawn_named(program: &str) -> Result<(), String> {
        let mut command = std::process::Command::new(program);
        #[cfg(windows)]
        box_foundation::suppress_console_window(&mut command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let _ = command.process_group(0);
        }
        command.spawn().map(|_| ()).map_err(|error| error.to_string())
    }

    /// Send one JSON request via HTTP POST /rpc; returns the parsed response frame.
    fn exchange(&self, request: Value) -> Result<RpcResponse, String> {
        self.exchange_within(request, RPC_READ_TIMEOUT)
    }

    /// `exchange` with an explicit read timeout, for callers that must not wait.
    fn exchange_within(&self, request: Value, timeout: Duration) -> Result<RpcResponse, String> {
        let addr = format!("127.0.0.1:{}", self.port);
        let mut stream = TcpStream::connect(&addr)
            .map_err(|error| format!("cannot connect to dshboxd at {}: {error}", addr))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("cannot set a read timeout on {addr}: {error}"))?;
        stream
            .set_write_timeout(Some(RPC_WRITE_TIMEOUT))
            .map_err(|error| format!("cannot set a write timeout on {addr}: {error}"))?;

        let body = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        let request_line = format!(
            "POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            self.token,
            body
        );
        stream
            .write_all(request_line.as_bytes())
            .map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())?;

        let mut reader = BufReader::new(stream);
        let mut response_str = String::new();
        reader.read_to_string(&mut response_str).map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                format!(
                    "dshboxd did not answer within {}s (it may be busy); retry, or check the daemon",
                    timeout.as_secs()
                )
            } else {
                format!("dshboxd read error: {error}")
            }
        })?;

        let mut boundary = None;
        for (i, _) in response_str.match_indices("\r\n\r\n") {
            boundary = Some(i + 4);
            break;
        }
        let boundary = match boundary {
            Some(pos) => pos,
            None => return Err("dshboxd response parse error: missing header/body boundary".to_string()),
        };

        let status_line = response_str[..boundary].lines().next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 200") {
            let body_part = &response_str[boundary..];
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(body_part);
            if let Ok(val) = parsed {
                let err = val["error"].as_str().unwrap_or("unknown error");
                return Err(err.to_string());
            }
            return Err(format!("dshboxd returned: {}", status_line));
        }

        let body_part = &response_str[boundary..];
        serde_json::from_str(body_part)
            .map_err(|error| format!("dshboxd response error: {error}"))
    }

    /// Call a method with JSON params; returns the `result` field (sync) or
/// the `task` field (async), whichever the daemon set. Error replies fall
/// through to the daemon's message.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut request = serde_json::json!({
            "token": self.token,
            "method": method,
        });
        if let Some(object) = params.as_object() {
            for (key, value) in object {
                request[key] = value.clone();
            }
        }
        let response = self.exchange(request)?;
        if response.ok {
            Ok(response
                .result
                .or(response.task)
                .unwrap_or(Value::Null))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "unknown daemon error".to_owned()))
        }
    }

    /// Enqueue an async task and return the task record. Equivalent to
    /// `call(method, params)` for async methods, but type-checking the
    /// presence of the `task` field gives a clearer error if the daemon
    /// replied synchronously.
    pub fn enqueue(&self, method: &str, params: Value) -> Result<Value, String> {
        let value = self.call(method, params)?;
        if value.is_null() {
            return Err(format!(
                "daemon replied without a task record for async method `{method}`"
            ));
        }
        Ok(value)
    }

    /// Health probe: the daemon answers without a token check failure.
    pub fn ping(&self) -> Result<Value, String> {
        self.call("ping", serde_json::json!({}))
    }

    /// A liveness probe that gives up quickly: callers poll it, so "not yet" has
    /// to come back in time to poll again.
    pub fn ping_quickly(&self) -> Result<Value, String> {
        let request = serde_json::json!({ "token": self.token, "method": "ping" });
        let response = self.exchange_within(request, PING_TIMEOUT)?;
        if response.ok {
            Ok(response.result.or(response.task).unwrap_or(Value::Null))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "unknown daemon error".to_owned()))
        }
    }
}
#[cfg(test)]
mod tests {
    // `Read` arrives with `super::*`: the module already imports it for
    // BufReader, so repeating it here only earns an unused-import warning.
    use super::*;
    use std::net::TcpListener;

    /// A daemon that accepts the connection and then says nothing must not hold
    /// the caller forever: a Tauri command thread would stay pending, and the
    /// CLI would look hung.
    #[test]
    fn a_silent_daemon_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and read the request, never reply.
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let client = RpcClient {
            port,
            token: "test".to_owned(),
        };
        let started = std::time::Instant::now();
        let error = client
            .exchange_within(
                serde_json::json!({ "token": "test", "method": "ping" }),
                Duration::from_millis(300),
            )
            .expect_err("a silent daemon is an error, not a value");
        assert!(
            error.contains("did not answer within"),
            "the error says what happened: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "it gives up on time, not when the daemon finally speaks"
        );
    }

    /// The same guard on the polling probe: a hung daemon answers "not yet" fast
    /// enough for the next poll.
    #[test]
    fn the_quick_probe_does_not_wait_for_a_stalled_daemon() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let client = RpcClient {
            port,
            token: "test".to_owned(),
        };
        let started = std::time::Instant::now();
        assert!(client.ping_quickly().is_err());
        assert!(
            started.elapsed() < PING_TIMEOUT + Duration::from_secs(1),
            "a poll returns inside its own budget"
        );
    }
}
