//! Thin JSON-over-unix-socket client for the `dshboxd` daemon.
//!
//! Used by the `dshbox` CLI (and, in a later phase, the desktop app) so
//! every client talks to the daemon instead of running business logic in
//! its own process. `connect()` reads the discovery file written by the
//! daemon and, when the daemon is not running, attempts to spawn it from
//! `PATH` and waits for it to come up.

use box_server_core::{default_endpoint, read_discovery, ServerDiscovery};
use serde_json::Value;

/// Response frame every daemon method returns.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RpcResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// A connected client bound to one daemon discovery.
#[derive(Clone)]
pub struct RpcClient {
    endpoint: String,
    token: String,
}

impl RpcClient {
    /// Build a client from an existing discovery record.
    pub fn from_discovery(discovery: &ServerDiscovery) -> Self {
        Self {
            endpoint: discovery.endpoint.clone(),
            token: discovery.token.clone(),
        }
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
                    "failed to reach dshboxd at {}; start it with: dshboxd &",
                    default_endpoint().display()
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

    /// Send one JSON-line request; returns the parsed response frame.
    #[cfg(unix)]
    fn exchange(&self, request: Value) -> Result<RpcResponse, String> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.endpoint)
            .map_err(|error| format!("cannot connect to dshboxd at {}: {error}", self.endpoint))?;
        let body = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        stream
            .write_all(body.as_bytes())
            .map_err(|error| error.to_string())?;
        stream.write_all(b"\n").map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("dshboxd read error: {error}"))?;
        serde_json::from_str(&line)
            .map_err(|error| format!("dshboxd response error: {error}"))
    }

    #[cfg(windows)]
    fn exchange(&self, _request: Value) -> Result<RpcResponse, String> {
        Err("dshboxd named-pipe transport is not yet implemented for Windows".to_owned())
    }

    /// Call a method with JSON params; returns the `result` field or the
    /// daemon's error message.
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
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "unknown daemon error".to_owned()))
        }
    }

    /// Health probe: the daemon answers without a token check failure.
    pub fn ping(&self) -> Result<Value, String> {
        self.call("ping", serde_json::json!({}))
    }
}
