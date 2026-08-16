//! Container lifecycle for the daemon: start the DSH host process for a
//! container, wait for it to become ready, and stop it. Mirrors the
//! desktop's `lifecycle.rs` without any Tauri dependency; the daemon owns
//! the child processes and the running registry.

use crate::containers::{
    ensure_container_workspace, preflight_profile_plugins, repair_known_profile_template,
    write_dshbox_context_snapshot,
};
use crate::image::lookup_template_path;
use crate::state::{ContainerManager, ManagedHost};
use crate::toolchains::{
    command_for_toolchain, resolve_toolchain, spawn_forwarding_log, wait_for_process,
};
use box_containers::container_directory;
use box_dsh_context::PLUGIN_ID;
use box_dsh_versions::version_directory as dsh_version_directory;
use box_foundation::{is_safe_identifier, read_config};
use box_scheduler::TaskContext;
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

/// Start the DSH host for `id` and wait until its frontend answers.
/// The running map is the daemon-owned registry; containers already
/// running return their existing URL.
pub(crate) fn start_dsh_container_inner(
    id: &str,
    running: &Mutex<BTreeMap<String, ManagedHost>>,
    task: Option<&TaskContext>,
) -> Result<String, String> {
    if !is_safe_identifier(id) {
        return Err("invalid container id".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    // Startup contract: every container must be based on a template (or its
    // `image` alias). The referenced template must still resolve through the
    // hash index (built templates live in `templates/<fnv1a64>/list.json`,
    // not as a flat `<name>.dsh` file — the legacy filename lookup would
    // miss them and report `template not found` even though the container
    // was materialised correctly moments before). `lookup_template_path`
    // falls back to the legacy alias for older installs.
    match value["template"].as_str() {
        Some(name) => {
            lookup_template_path(&root, name).map_err(|error| {
                format!("template not found: {name} ({error})")
            })?;
        }
        None => {
            if value["image"].as_str().is_none() {
                return Err("container is not based on a template or image".to_owned());
            }
        }
    }
    let version = value["version"]
        .as_str()
        .ok_or("container has no version")?;
    let profile = value["profile"].as_str().unwrap_or("web");
    repair_known_profile_template(&directory, profile)?;
    let workspace = ensure_container_workspace(&directory)?;
    let context_files = write_dshbox_context_snapshot(&directory, &value, profile)?;
    // DSH's Cordis loader imports loader entries through Node's ESM
    // machinery, which never consults NODE_PATH; expose the vendored
    // plugin as a real node_modules entry next to the profile.
    ensure_bundled_context_plugin(&directory, profile)?;
    let source = dsh_version_directory(&root, version);
    if !source.join("package.json").is_file() {
        return Err("DSH source is incomplete".to_owned());
    }
    {
        let mut running = running
            .lock()
            .map_err(|_| "container manager lock failed")?;
        if let Some(host) = running.get_mut(id) {
            if host
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(host.url.clone());
            }
        }
    }
    if let Some(task) = task {
        task.update("Preparing DSH host", 25);
        task.check_cancelled()?;
    }
    preflight_profile_plugins(&directory, profile, task)?;
    if let Some(task) = task {
        task.check_cancelled()?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot allocate port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);
    let patch = directory.join("box-web.patch.yml");
    fs::write(
        &patch,
        format!("- id: webserver\n  config:\n    host: 127.0.0.1\n    port: {port}\n"),
    )
    .map_err(|error| format!("cannot write web patch: {error}"))?;
    let pnpm = resolve_toolchain("pnpm")?;
    let source_arg = source.to_string_lossy().into_owned();
    let url = format!("http://127.0.0.1:{port}");
    let log_path = directory.join("logs").join("host.log");
    let _ = fs::File::create(&log_path);
    let mut built = source
        .join("apps")
        .join("web")
        .join("dist")
        .join("index.html")
        .is_file();
    let mut attempt = 0;
    loop {
        attempt += 1;
        if !built {
            if !source.join("node_modules").is_dir() {
                if let Some(task) = task {
                    task.update("Installing DSH dependencies", 40);
                    task.log("installing DSH dependencies");
                }
                let mut install = command_for_toolchain(&pnpm);
                install.args(["--dir", source_arg.as_ref(), "install"]);
                let mut install = spawn_forwarding_log(&mut install, &log_path, task)
                    .map_err(|error| format!("cannot install DSH dependencies: {error}"))?;
                let status = wait_for_process(&mut install, task, "installing DSH dependencies")?;
                if !status.success() {
                    return Err(format!(
                        "DSH dependency install failed; inspect {}",
                        log_path.display()
                    ));
                }
                if let Some(task) = task {
                    task.check_cancelled()?;
                }
            }
            if let Some(task) = task {
                task.update("Building DSH frontend", 55);
                task.log("building DSH frontend");
            }
            let mut build = command_for_toolchain(&pnpm);
            build.args(["--dir", source_arg.as_ref(), "run", "build"]);
            let mut build = spawn_forwarding_log(&mut build, &log_path, task)
                .map_err(|error| format!("cannot build DSH before launch: {error}"))?;
            let status = wait_for_process(&mut build, task, "building DSH frontend")?;
            if !status.success() {
                return Err(format!("DSH build failed; inspect {}", log_path.display()));
            }
            if let Some(task) = task {
                task.check_cancelled()?;
            }
            built = true;
        }
        if let Some(task) = task {
            task.update("Launching DSH host", 75);
            task.log("launching DSH host");
        }
        let plugins_node_modules =
            PathBuf::from(&root).join("plugins").join("node_modules");
        let mut command = command_for_toolchain(&pnpm);
        command
            .args([
                "--dir",
                source.to_string_lossy().as_ref(),
                "dsh",
                "--profile",
                profile,
                "--patch",
                context_files.patch_path.to_string_lossy().as_ref(),
                "--patch",
                patch.to_string_lossy().as_ref(),
            ])
            .current_dir(&workspace)
            .env("DSH_HOME", directory.join("profile"))
            .env("NODE_PATH", plugins_node_modules.as_os_str());
        let mut child = spawn_forwarding_log(&mut command, &log_path, task)
            .map_err(|error| format!("cannot start DSH host: {error}"))?;
        let ready = (0..80).any(|attempt| {
            if task.map(TaskContext::cancelled).unwrap_or(false) {
                crate::toolchains::kill_process_tree(child.id());
                let _ = child.wait();
                return false;
            }
            if child.try_wait().ok().flatten().is_some() {
                return false;
            }
            if let Some(task) = task {
                if attempt % 10 == 0 {
                    task.log(&format!("waiting for DSH host ({}/20s)", attempt / 4));
                }
            }
            let available = reqwest::blocking::get(&url)
                .map(|response| response.status().is_success())
                .unwrap_or(false);
            if !available {
                thread::sleep(Duration::from_millis(250));
            }
            available
        });
        if ready {
            let tree = Arc::new(Mutex::new(Vec::new()));
            let collector_tree = tree.clone();
            let root_pid = child.id();
            std::thread::spawn(move || {
                collect_process_descendants(root_pid, collector_tree, Duration::from_secs(2));
            });
            let pid_path =
                PathBuf::from(&directory).join("state").join("host.pid");
            if let Some(parent) = pid_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(error) = fs::write(&pid_path, child.id().to_string()) {
                if let Some(task) = task {
                    task.log(&format!("warning: cannot write host pid file: {error}"));
                }
            }
            running
                .lock()
                .map_err(|_| "container manager lock failed")?
                .insert(
                    id.to_owned(),
                    ManagedHost {
                        child,
                        url: url.clone(),
                        tree,
                    },
                );
            if let Some(task) = task {
                task.update("DSH host is ready", 95);
            }
            return Ok(url);
        }
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            return Err("task cancelled while waiting for DSH host".to_owned());
        }
        crate::toolchains::kill_process_tree(child.id());
        let _ = child.wait();
        if attempt == 1 && built {
            if let Some(task) = task {
                task.update("Rebuilding DSH frontend", 60);
                task.log("DSH launch failed; rebuilding frontend");
            }
            built = false;
            continue;
        }
        let pid_path = PathBuf::from(&directory).join("state").join("host.pid");
        let _ = fs::remove_file(&pid_path);
        return Err(format!(
            "DSH host did not become ready; inspect {}",
            log_path.display()
        ));
    }
}

/// Stop a running container host: kill the recorded tree and drop the
/// persisted PID file.
pub(crate) fn stop_dsh_container(
    id: &str,
    manager: &ContainerManager,
) -> Result<(), String> {
    let host = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(id);
    if let Some(mut host) = host {
        crate::toolchains::kill_process_tree(host.child.id());
        let _ = host.child.wait();
        let descendants = host
            .tree
            .lock()
            .map(|tree| tree.clone())
            .unwrap_or_default();
        for &pid in &descendants {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let pid_path = container_directory(&root, id).join("state").join("host.pid");
    let _ = fs::remove_file(&pid_path);
    Ok(())
}

/// Rebuild a container's DSH frontend: stop the host, reinstall and build
/// the harness source, then start the host again. Mirrors the desktop's
/// `rebuild_dsh_container_with_task` without Tauri dependencies.
pub(crate) fn rebuild_dsh_container_with_task(
    id: String,
    manager: &ContainerManager,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    if let Some(task) = task {
        task.update("Stopping DSH host", 20);
    }
    stop_dsh_container(&id, manager)?;
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    let source = dsh_version_directory(
        &root,
        value["version"]
            .as_str()
            .ok_or("container has no version")?,
    );
    let pnpm = resolve_toolchain("pnpm")?;
    let log_path = directory.join("logs").join("rebuild.log");
    fs::File::create(&log_path).map_err(|error| format!("cannot create rebuild log: {error}"))?;
    let source_arg = source.to_string_lossy().into_owned();
    for (index, args) in [
        ["--dir", source_arg.as_ref(), "install"],
        ["--dir", source_arg.as_ref(), "build"],
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(task) = task {
            task.update(
                if index == 0 {
                    "Installing DSH dependencies"
                } else {
                    "Building DSH frontend"
                },
                if index == 0 { 45 } else { 70 },
            );
            task.check_cancelled()?;
        }
        let mut command = command_for_toolchain(&pnpm);
        command.args(args);
        let mut command = spawn_forwarding_log(&mut command, &log_path, task)
            .map_err(|error| format!("cannot run pnpm: {error}"))?;
        let status = wait_for_process(&mut command, task, "running pnpm")?;
        if !status.success() {
            return Err(format!(
                "DSH rebuild failed; inspect {}",
                log_path.display()
            ));
        }
    }
    start_dsh_container_inner(&id, &manager.running, task).map(|_| ())
}

/// Expose the vendored `@deepseek-ai/dsh-box-context` bundle to the DSH
/// loader as a real `node_modules` entry under the container's profile.
fn ensure_bundled_context_plugin(directory: &Path, profile: &str) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let vendored = PathBuf::from(&root)
        .join("plugins")
        .join("node_modules")
        .join("@deepseek-ai")
        .join(PLUGIN_ID);
    if !vendored.join("package.json").is_file() {
        // Self-heal before giving up: the config's manifest digest may
        // predate the current storage root (the vendoring copy never
        // landed here), so re-run the vendoring once. `initialize_bundled
        // _plugins` verifies the tree in the current directory itself.
        let _ = crate::state::initialize_bundled_plugins();
    }
    if !vendored.join("package.json").is_file() {
        // Vendored plugin tree is still missing (e.g. a developer build
        // that skipped the plugin bundler); let DSH surface the resolution
        // error if the patch still references the bundle.
        return Ok(());
    }
    let profile_node_modules = directory
        .join("profile")
        .join("profiles")
        .join(profile)
        .join("node_modules");
    link_vendored_plugin(&vendored, &profile_node_modules)
}

/// Idempotent exposure of the vendored bundle under a profile's
/// `node_modules`. Preferred shape is a directory symlink; a failed link
/// falls back to a recursive copy, kept fresh by comparing `package.json`.
fn link_vendored_plugin(vendored: &Path, profile_node_modules: &Path) -> Result<(), String> {
    let scoped = profile_node_modules.join("@deepseek-ai");
    fs::create_dir_all(&scoped)
        .map_err(|error| format!("cannot create {}: {error}", scoped.display()))?;
    let link = scoped.join(PLUGIN_ID);
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(&link).map(|target| target == vendored).unwrap_or(false) {
                return Ok(());
            }
            fs::remove_file(&link)
                .map_err(|error| format!("cannot replace stale plugin link: {error}"))?;
        }
        Ok(_) => {
            if files_equal(&vendored.join("package.json"), &link.join("package.json")) {
                return Ok(());
            }
            fs::remove_dir_all(&link)
                .map_err(|error| format!("cannot replace stale plugin dir: {error}"))?;
        }
        Err(_) => {}
    }
    if let Err(link_error) = create_directory_symlink(vendored, &link) {
        copy_dir_recursive(vendored, &link).map_err(|copy_error| {
            format!(
                "cannot link {} (symlink: {link_error}; copy: {copy_error})",
                link.display()
            )
        })?;
    }
    Ok(())
}

fn files_equal(first: &Path, second: &Path) -> bool {
    fs::read(first)
        .ok()
        .zip(fs::read(second).ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false)
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(symlink_error) => {
            // Directory junctions (`mklink /J`) need no Developer Mode or
            // elevation; the caller falls back to a recursive copy if even
            // this fails.
            let link_arg = format!("\"{}\"", link.display());
            let target_arg = format!("\"{}\"", target.display());
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &link_arg, &target_arg])
                .output()?;
            if output.status.success() {
                return Ok(());
            }
            Err(std::io::Error::other(format!(
                "symlink_dir: {symlink_error}; junction: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// Recursive directory copy (Windows fallback for directory symlinks).
fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Watches the process table for a short window and records every descendant
/// of `root` into `tree`, so Stop can finish the job on orphaned hosts.
fn collect_process_descendants(root: u32, tree: Arc<Mutex<Vec<u32>>>, total: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < total {
        let procs = process_table();
        if let Ok(mut guard) = tree.lock() {
            let mut frontier = vec![root];
            while let Some(parent) = frontier.pop() {
                for &(pid, ppid) in &procs {
                    if pid != root && ppid == parent && !guard.contains(&pid) {
                        guard.push(pid);
                        frontier.push(pid);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
}

/// Snapshot of (pid, parent pid) pairs for every process on the system.
#[cfg(target_os = "windows")]
fn process_table() -> Vec<(u32, u32)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut table = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return table;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                table.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    table
}

/// Snapshot of (pid, parent pid) pairs read from `/proc` (Linux). macOS has
/// no /proc, so the read fails and the table stays empty — a safe no-op.
#[cfg(not(target_os = "windows"))]
fn process_table() -> Vec<(u32, u32)> {
    let mut table = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // stat format: pid (comm) state ppid ...; comm may contain spaces.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let Some(ppid) = stat[close + 1..]
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        table.push((pid, ppid));
    }
    table
}
