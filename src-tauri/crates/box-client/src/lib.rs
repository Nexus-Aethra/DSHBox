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

    /// Best-effort spawn of the daemon from `PATH`.
    pub fn spawn_daemon() -> Result<(), String> {
        let mut command = std::process::Command::new("dshboxd");
        #[cfg(windows)]
        box_foundation::suppress_console_window(&mut command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let _ = command.process_group(0);
        }
        command
            .spawn()
            .map_err(|error| format!("cannot start dshboxd: {error}"))?;
        Ok(())
    }

    /// Send one JSON request via HTTP POST /rpc; returns the parsed response frame.
    fn exchange(&self, request: Value) -> Result<RpcResponse, String> {
        let addr = format!("127.0.0.1:{}", self.port);
        let mut stream = TcpStream::connect(&addr)
            .map_err(|error| format!("cannot connect to dshboxd at {}: {error}", addr))?;

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
        reader
            .read_to_string(&mut response_str)
            .map_err(|error| format!("dshboxd read error: {error}"))?;

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
}