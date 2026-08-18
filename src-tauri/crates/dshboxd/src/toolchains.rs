//! Toolchain resolution for daemon-run tasks. Mirrors the desktop's
//! `toolchains.rs` but resolves against the daemon-owned bundled runtime.

use crate::state::bundled_runtime;
use box_foundation::{read_config, suppress_console_window};
use box_scheduler::TaskContext;
use box_toolchains::is_known_toolchain;
use serde::Serialize;
use std::{
    ffi::OsString,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedToolchain {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) arguments: Vec<String>,
}

pub(crate) fn resolve_toolchain(id: &str) -> Result<ResolvedToolchain, String> {
    if !is_known_toolchain(id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    let runtime = bundled_runtime()?;
    let (path, arguments) = match id {
        "node" => (runtime.node.clone(), Vec::new()),
        "npm" => (
            runtime.node.clone(),
            vec![runtime.npm.to_string_lossy().into_owned()],
        ),
        "pnpm" => (
            runtime.node.clone(),
            vec![runtime.pnpm.to_string_lossy().into_owned()],
        ),
        _ => return Err(format!("unsupported bundled toolchain: {id}")),
    };
    Ok(ResolvedToolchain {
        id: id.to_owned(),
        source: "bundled".to_owned(),
        path: path.to_string_lossy().into_owned(),
        arguments,
    })
}

pub(crate) fn command_for_toolchain(toolchain: &ResolvedToolchain) -> Command {
    let mut command = Command::new(&toolchain.path);
    suppress_console_window(&mut command);
    command.args(&toolchain.arguments);
    // Prepend the bundled runtime bin directories so child processes can
    // resolve bare `pnpm`/`npm` commands.
    if let Ok(runtime) = bundled_runtime() {
        if let Some(node_dir) = runtime.node.parent() {
            let pnpm_dir = node_dir.parent().map(|root| root.join("pnpm"));
            if let Some(existing) = std::env::var_os("PATH") {
                let mut parts: Vec<OsString> = vec![node_dir.as_os_str().to_owned()];
                if let Some(pnpm_dir) = pnpm_dir {
                    parts.push(pnpm_dir.as_os_str().to_owned());
                }
                parts.push(existing);
                if let Ok(joined) = std::env::join_paths(parts) {
                    command.env("PATH", joined);
                }
            }
        }
    }
    if let Ok(config) = read_config() {
        if let Some(registry) = config.npm_registry.as_deref() {
            command.env("npm_config_registry", registry);
        }
        // Pin pnpm's store under DSHBox's runtime directory so the cache
        // moves with the install and is removed on uninstall — the default
        // `~/.local/share/pnpm/store` would otherwise scatter outside of
        // DSHBox's control.
        if let Some(runtime_dir) = config.runtime_directory.as_deref() {
            let pnpm_root = PathBuf::from(runtime_dir).join("pnpm");
            let _ = std::fs::create_dir_all(&pnpm_root);
            command.env("PNPM_STORE_DIR", pnpm_root.join("store"));
            // npm is used by `fetch_extension_via_npm_pack` for GitHub/Git
            // URL plugin sources. Without pinning its cache, `npm pack`
            // leaks into `~/.npm/_cacache/` and can collide with other
            // npm instances when doing concurrent `git --mirror` clones.
            let npm_cache = pnpm_root.join("npm-cache");
            let _ = std::fs::create_dir_all(&npm_cache);
            command.env("npm_config_cache", npm_cache);
        }
    }
    command
}

pub(crate) fn wait_for_process(
    child: &mut Child,
    task: Option<&TaskContext>,
    description: &str,
) -> Result<std::process::ExitStatus, String> {
    loop {
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            kill_process_tree(child.id());
            let _ = child.wait();
            return Err(format!("task cancelled while {description}"));
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawns a child with piped output and forwards every line to both the
/// given log file and the task's live log.
pub(crate) fn spawn_forwarding_log(
    command: &mut Command,
    log_file: &Path,
    task: Option<&TaskContext>,
) -> Result<Child, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("missing piped stdout")?;
    let stderr = child.stderr.take().ok_or("missing piped stderr")?;
    let log_file = log_file.to_path_buf();
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let task = task.cloned();
        let log_file = log_file.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_file)
                    .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
                if let Some(task) = &task {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        task.log(trimmed);
                    }
                }
                line.clear();
            }
        });
    }
    Ok(child)
}

pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("taskkill");
        suppress_console_window(&mut command);
        let _ = command
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}
