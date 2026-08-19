//! Subscribe to the daemon's `/events` SSE stream and re-broadcast each
//! event on the Tauri event bus as `daemon://event`. The desktop is a thin
//! presentation client; the daemon owns task state and emits every state
//! transition here, so this module is the only place we touch the wire.
//!
//! Implementation notes:
//! - Pure std (no reqwest): the daemon already speaks text/event-stream
//!   over plain TCP, and adding a runtime just for an SSE parser would
//!   inflate the desktop binary.
//! - The subscribe loop reconnects with a short backoff when the daemon
//!   restarts; the snapshot frame is the new "ground truth", so missing
//!   any intermediate events is safe.
//! - On Windows we disable Nagle's algorithm (`set_nodelay`) so log
//!   lines surface in the UI within tens of milliseconds instead of
//!   waiting on the kernel's coalescing timer.

use box_client::RpcClient;
use box_server_core::read_discovery;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Event name every daemon SSE frame is re-broadcast under. The payload
/// is the JSON object the daemon sent (`{"type": "...", ...}`).
pub(crate) const DAEMON_EVENT: &str = "daemon://event";

/// Spawn one daemon-event subscriber thread. The thread exits when the
/// app handle is dropped; failures are logged via `write_startup_log` so
/// the startup banner shows the first connection problem.
pub(crate) fn spawn_event_subscriber(app: AppHandle) {
    thread::Builder::new()
        .name("dshbox-events".into())
        .spawn(move || {
            if let Err(error) = run_loop(&app) {
                write_startup_log(&format!("daemon event subscriber stopped: {error}"));
            }
        })
        .ok();
}

fn run_loop(app: &AppHandle) -> Result<(), String> {
    let mut backoff = Duration::from_millis(250);
    loop {
        match subscribe_once(app) {
            Ok(()) => {
                backoff = Duration::from_millis(250);
            }
            Err(error) => {
                write_startup_log(&format!("daemon /events connection lost: {error}"));
            }
        }
        // Cap the backoff so a long daemon outage does not park the
        // subscriber for minutes when the daemon comes back up.
        backoff = (backoff * 2).min(Duration::from_secs(5));
        thread::sleep(backoff);
    }
}

/// Open one SSE connection and pump frames until the daemon closes it
/// or an I/O error occurs. Returns `Ok(())` on clean disconnect (which
/// the loop will treat as "reconnect immediately").
fn subscribe_once(app: &AppHandle) -> Result<(), String> {
    let Some(discovery) = read_discovery()
        .map_err(|error| format!("cannot read discovery: {error}"))?
    else {
        return Err("daemon discovery is not published yet".to_owned());
    };
    let client = RpcClient::from_discovery(&discovery);
    let addr = format!("127.0.0.1:{}", discovery.port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|error| format!("cannot connect to {addr}: {error}"))?;
    let _ = stream.set_nodelay(true);
    let request = format!(
        "GET /events?token={} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        client.token()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot write SSE request: {error}"))?;
    let mut reader = BufReader::new(stream);
    // Skip the HTTP status line + headers (terminated by a blank line).
    let mut header_line = String::new();
    loop {
        header_line.clear();
        let read = reader
            .read_line(&mut header_line)
            .map_err(|error| format!("cannot read SSE header: {error}"))?;
        if read == 0 {
            return Err("daemon closed SSE connection during handshake".to_owned());
        }
        if header_line == "\r\n" || header_line == "\n" {
            break;
        }
    }
    pump_frames(app, &mut reader)
}

/// Parse Server-Sent-Events frames until EOF. Each `data:` line is
/// concatenated (SSE allows multi-line data) and forwarded as one
/// `daemon://event` payload.
fn pump_frames<R: Read>(app: &AppHandle, reader: &mut BufReader<R>) -> Result<(), String> {
    let mut line = String::new();
    let mut data = String::new();
    let mut event = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("cannot read SSE frame: {error}"))?;
        if read == 0 {
            if !data.is_empty() {
                forward(app, event.as_str(), &data);
            }
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.is_empty() {
                forward(app, event.as_str(), &data);
                data.clear();
                event.clear();
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            event = rest.trim().to_owned();
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // Other SSE fields (id:, retry:, comments starting with `:`) are
        // ignored; the daemon does not emit any of them today.
    }
}

fn forward(app: &AppHandle, event: &str, data: &str) {
    let payload: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(error) => {
            write_startup_log(&format!(
                "skipping malformed SSE payload for event `{event}`: {error}"
            ));
            return;
        }
    };
    let _ = app.emit(
        DAEMON_EVENT,
        serde_json::json!({
            "event": event,
            "payload": payload,
        }),
    );
}

#[allow(dead_code)]
fn _unused_stub() {
    // Kept so future helpers have a place to land without needing an
    // immediate use site.
}

use crate::desktop::write_startup_log;