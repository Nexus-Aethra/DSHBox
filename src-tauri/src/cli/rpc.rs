//! Thin-client RPC helpers: connect to the daemon and run tasks through
//! it. The CLI never owns business state; every command serializes a
//! request and prints what the daemon reports.

use box_client::RpcClient;
use box_scheduler::TaskRecord;
use serde_json::{json, Value};
use std::{
    io::{IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// Connect to the daemon, spawning `dshboxd` from `PATH` when needed.
/// While a freshly-spawned daemon comes up (up to a few seconds) a spinner
/// is drawn on stderr so the wait is visible; it is suppressed when stderr
/// is not a terminal.
pub(crate) fn connect() -> Result<RpcClient, String> {
    if daemon_alive() {
        return RpcClient::connect();
    }
    spinner_while("starting dshboxd", RpcClient::connect)
}

/// True when a daemon is already reachable, so `connect()` skips the
/// spawn-and-poll path (and its spinner) entirely.
fn daemon_alive() -> bool {
    box_server_core::read_discovery()
        .ok()
        .flatten()
        .map(|discovery| RpcClient::from_discovery(&discovery).ping().is_ok())
        .unwrap_or(false)
}

/// Run `work` while animating a spinner on stderr. The spinner is skipped
/// when stderr is not a terminal, and the line is cleared before returning
/// so the next output starts on a fresh line.
fn spinner_while<T>(label: &'static str, work: impl FnOnce() -> T) -> T {
    if !std::io::stderr().is_terminal() {
        return work();
    }
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let stop = Arc::new(AtomicBool::new(false));
    let spinner_stop = Arc::clone(&stop);
    let spinner = thread::spawn(move || {
        let mut index = 0usize;
        while !spinner_stop.load(Ordering::Relaxed) {
            let mut stderr = std::io::stderr().lock();
            let _ = write!(stderr, "\r{} {label} ", FRAMES[index % FRAMES.len()]);
            let _ = stderr.flush();
            drop(stderr);
            index += 1;
            thread::sleep(Duration::from_millis(80));
        }
    });
    let result = work();
    stop.store(true, Ordering::Relaxed);
    let _ = spinner.join();
    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\r\x1b[2K");
    let _ = stderr.flush();
    result
}

/// Call one daemon method and return the `result` value.
pub(crate) fn call(client: &RpcClient, method: &str, params: Value) -> Result<Value, String> {
    client.call(method, params)
}

/// Resolve a user-supplied path against the client's working directory
/// before it is serialized: the daemon runs with a different CWD, so every
/// path argument must be absolute. URLs and absolute paths pass through.
pub(crate) fn absolutize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("github.com/")
        || std::path::Path::new(trimmed).is_absolute()
    {
        return trimmed.to_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(trimmed).to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.to_owned())
}

/// Enqueue a task through a daemon RPC method (the daemon spawns its own
/// worker) and block until it finishes, printing `[{progress}%] {stage}`
/// and log lines to stderr so stdout stays reserved for command output.
pub(crate) fn run_task(client: &RpcClient, method: &str, params: Value) -> Result<(), String> {
    let value = client.call(method, params)?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    wait_task(client, &task.id)
}

/// Poll `task_status` until the task settles, printing progress and log
/// lines to stderr.
pub(crate) fn wait_task(client: &RpcClient, task_id: &str) -> Result<(), String> {
    let mut last_stage = String::new();
    let mut log_offset = 0usize;
    loop {
        let value = client.call("task_status", json!({ "id": task_id }))?;
        let task: TaskRecord = serde_json::from_value(value)
            .map_err(|error| format!("invalid task record from daemon: {error}"))?;
        if task.stage != last_stage {
            eprintln!("[{:>3}%] {}", task.progress, task.stage);
            last_stage = task.stage.clone();
        }
        if let Ok(content) = std::fs::read_to_string(&task.log_path) {
            if content.len() > log_offset {
                for line in content[log_offset..].lines() {
                    eprintln!("  {line}");
                }
                log_offset = content.len();
            }
        }
        match task.status.as_str() {
            "succeeded" => return Ok(()),
            "cancelled" => return Err("task cancelled".to_owned()),
            "failed" => {
                return Err(task
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("task failed at {}", task.stage)))
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}
