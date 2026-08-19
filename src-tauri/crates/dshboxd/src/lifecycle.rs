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
use crate::toolchains::{pnpm_policy, resolve_toolchain, run_logged, TaskCancel};
use box_containers::container_directory;
use box_dsh_context::PLUGIN_ID;
use box_dsh_versions::version_directory as dsh_version_directory;
use box_foundation::{is_safe_identifier, read_config};
use box_runtime::process::{self, ExecutionKind, ProcessSpec, TrackedChild};
use box_scheduler::TaskContext;
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use crate::host::{self, HostState};

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
    let name = value["name"].as_str().unwrap_or(id).to_owned();
    let template = value["template"].as_str().map(str::to_owned);
    repair_known_profile_template(&directory, profile)?;
    let _workspace = ensure_container_workspace(&directory)?;
    let dshbox_home = crate::state::dshbox_install_directory()
        .unwrap_or_else(|_| PathBuf::from("."));
    let context_files = write_dshbox_context_snapshot(&directory, &value, profile, &dshbox_home)?;
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
    // Write a Corrupted sentinel before preflight: if plugin build
    // fails here, the container still has a host.json so the UI can
    // distinguish "failed to prepare" from "user stopped it". Once the
    // host actually spawns, we overwrite it with the Starting record.
    let _ = std::fs::create_dir_all(directory.join("state"))
        .map_err(|error| format!("cannot create host state directory: {error}"));
    let _ = host::write_corrupted_record(
        id,
        &name,
        template.as_deref(),
        profile,
        "container created; preflight not yet complete",
    );
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
                let install_spec = ProcessSpec::new(pnpm.path.clone())
                    .args(&pnpm.arguments)
                    .args(["--dir", source_arg.as_ref(), "install"])
                    .policy(pnpm_policy(&pnpm))
                    .kind(ExecutionKind::Logged)
                    .log_path(&log_path);
                let mut install_logged = run_logged(&install_spec, "DSH dependency install").map_err(|error| format!("cannot install DSH dependencies: {error}"))?;
                let status = install_logged
                    .wait_or_kill(&TaskCancel(task), Duration::from_secs(900), "installing DSH dependencies")
                    .map_err(|error| format!("DSH dependency install: {error}"))?;
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
            let build_spec = ProcessSpec::new(pnpm.path.clone())
                .args(&pnpm.arguments)
                .args(["--dir", source_arg.as_ref(), "run", "build"])
                .policy(pnpm_policy(&pnpm))
                .kind(ExecutionKind::Logged)
                .log_path(&log_path);
            let mut build_logged = run_logged(&build_spec, "DSH frontend build").map_err(|error| format!("cannot build DSH before launch: {error}"))?;
            let status = build_logged
                .wait_or_kill(&TaskCancel(task), Duration::from_secs(900), "building DSH frontend")
                .map_err(|error| format!("DSH build: {error}"))?;
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
        // Wait for the harness runtime's pnpm store to finish initialising.
        // `pull_template` only clones the repo — it does NOT run `pnpm
        // install` — so the first container create on a freshly-installed
        // harness races the on-demand `pnpm install` in the container-build
        // path above. If we spawn node before pnpm has linked `tsx` (and
        // the rest of the ESM deps) into `source/node_modules/`, Node v24's
        // ESM resolver reports `ERR_MODULE_NOT_FOUND` for the `--import`
        // specifier. Bound the wait to a few seconds — pnpm install for the
        // dsh monorepo typically lands within a second once started, so a
        // short poll is enough; if it takes longer the actual `pnpm install`
        // failure will surface through the build step above and abort
        // before we get here.
        wait_for_pnpm_links(&source, std::time::Duration::from_secs(15))
            .map_err(|error| format!("DSH runtime not ready ({error})"))?;
        // Launch the DSH host directly via `node --import tsx/esm` instead of
        // going through `pnpm dsh`.  pnpm's lifecycle runner wraps the exit
        // code in `[ELIFECYCLE]` and swallows the actual error message,
        // making it impossible to diagnose startup failures.  Running the
        // script directly lets the node process's stderr propagate to the
        // host.log unmodified.
        let node = resolve_toolchain("node")?;
        let policy = process::dsh_host_policy(
            std::path::Path::new(&node.path).parent().map(Path::to_path_buf).as_deref().unwrap_or(Path::new(".")),
            std::path::Path::new(&node.path).parent().and_then(|p| p.parent()).map(|p| p.join("pnpm")).as_deref().unwrap_or(Path::new(".")),
            &plugins_node_modules,
        );
        let policy = policy.task_override("DSH_HOME", directory.join("profile").to_string_lossy().into_owned());
        let spec = ProcessSpec::new(node.path.clone())
            .args([
                "--import",
                "tsx/esm",
                source.join("apps/cli/src/bin.ts").to_string_lossy().as_ref(),
                "--profile",
                profile,
                "--patch",
                context_files.patch_path.to_string_lossy().as_ref(),
                "--patch",
                patch.to_string_lossy().as_ref(),
            ])
            .cwd(&source)
            .policy(policy)
            .kind(ExecutionKind::Logged)
            .log_path(&log_path)
            .new_process_group(true);
        let mut tracked: TrackedChild = run_logged(&spec, "DSH host")
            .map_err(|error| format!("cannot start DSH host: {error}"))?
            .into_tracked();
        // Detach into a TrackedChild for stop / graceful_shutdown handling.
        let host_pid = tracked.id().unwrap_or(0);
        let host_pgid = tracked.pgid().unwrap_or(0);
        // Persist a starting record before the readiness probe begins so
        // a daemon crash mid-start still leaves something for the next
        // run to reconcile against.
        let initial = host::initial_record(
            id,
            &name,
            template.as_deref(),
            profile,
            host_pid,
            host_pgid,
            port,
            &url,
        );
        let _ = host::write_host_record(&initial);
        let pid_path = PathBuf::from(&directory).join("state").join("host.pid");
        if let Some(parent) = pid_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(&pid_path, host_pid.to_string()) {
            if let Some(task) = task {
                task.log(&format!("warning: cannot write host pid file: {error}"));
            }
        }
        let ready = (0..80).any(|attempt| {
            if task.map(TaskContext::cancelled).unwrap_or(false) {
                let _ = tracked.kill_tree(false, Duration::from_secs(2));
                return false;
            }
            if let Ok(Some(status)) = tracked.try_wait() {
                let (code, signal) = host::exit_status_to_parts(status);
                if let Some(task) = task {
                    if let Some(sig) = signal {
                        task.log(&format!(
                            "DSH host exited early (code {code}, signal {sig})"
                        ));
                    } else {
                        task.log(&format!("DSH host exited early (code {code})"));
                    }
                }
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
            let snapshot = host::read_host_record(id)
                .ok()
                .flatten()
                .unwrap_or(initial.clone());
            let _ = host::compare_and_swap_host_record(id, &snapshot, |on_disk| {
                let mut next = on_disk.clone();
                next.state = HostState::Ready;
                next.last_seen = box_foundation::now_seconds();
                next
            });
            spawn_health_watcher(id, url.clone());
            running
                .lock()
                .map_err(|_| "container manager lock failed")?
                .insert(
                    id.to_owned(),
                    ManagedHost {
                        child: tracked,
                        url: url.clone(),
                    },
                );
            if let Some(task) = task {
                task.update("DSH host is ready", 95);
            }
            return Ok(url);
        }
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            let _ = host::compare_and_swap_host_record(id, &initial, |on_disk| {
                let mut next = on_disk.clone();
                next.state = HostState::Stopped;
                next
            });
            return Err("task cancelled while waiting for DSH host".to_owned());
        }
        let _ = tracked.kill_tree(false, Duration::from_secs(2));
        let (code, signal) = tracked
            .try_wait()
            .ok()
            .flatten()
            .map(host::exit_status_to_parts)
            .unwrap_or((-1, None));
        let _ = host::compare_and_swap_host_record(id, &initial, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Crashed;
            next.exit_status = Some(code);
            next.exit_signal = signal;
            next
        });
        if attempt == 1 && built {
            if let Some(task) = task {
                task.update("Rebuilding DSH frontend", 60);
                task.log("DSH launch failed; rebuilding frontend");
            }
            built = false;
            continue;
        }
        let _ = fs::remove_file(&pid_path);
        return Err(format!(
            "DSH host did not become ready; inspect {}",
            log_path.display()
        ));
    }
}

/// Stop a running container host: send SIGTERM to the process group,
/// wait up to 5s for graceful exit, escalate to SIGKILL, and update the
/// `host.json` state to `Stopped`.
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
        let _ = host.child.kill_tree(false, Duration::from_secs(5));
        // TrackedChild's Drop impl performs the final force-kill + reap
        // if the process hasn't exited yet, so we don't need an extra
        // wait() here.
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let pid_path = container_directory(&root, id).join("state").join("host.pid");
    let _ = fs::remove_file(&pid_path);
    if let Ok(Some(snapshot)) = host::read_host_record(id) {
        let _ = host::compare_and_swap_host_record(id, &snapshot, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Stopped;
            next
        });
    }
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
        let spec = ProcessSpec::new(pnpm.path.clone())
            .args(&pnpm.arguments)
            .args(args)
            .policy(pnpm_policy(&pnpm))
            .kind(ExecutionKind::Logged)
            .log_path(&log_path);
        let mut logged = run_logged(&spec, "pnpm rebuild step")?;
        let status = logged
            .wait_or_kill(&TaskCancel(task), Duration::from_secs(900), "running pnpm")?;
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
            let mut cmd = std::process::Command::new("cmd");
            box_foundation::suppress_console_window(&mut cmd);
            let output = cmd
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

/// Watches the host URL and writes back to `host.json`.
///
/// Every `PROBE_INTERVAL` (2s by default) the thread:
///   1. Reads the current record (snapshot).
///   2. Calls `try_wait` on the host PID via `kill -0`. ESRCH = dead,
///      bump `state` to `Crashed` with the captured exit info and exit
///      the loop.
///   3. Otherwise HTTP GETs the URL. On 2xx, bumps `last_seen` and
///      `probe_count`, resets `unhealthy_count`. On transport failure
///      or non-2xx, increments `unhealthy_count`; after `UNHEALTHY_THRESHOLD`
///      consecutive failures, marks the host `Crashed` and exits.
///
/// The watcher never auto-restarts — a `Crashed` record is a tombstone
/// the user clears with `dshbox container start` (which spawns a fresh
/// watcher for the new generation).
fn spawn_health_watcher(id: &str, url: String) {
    let id_owned = id.to_owned();
    std::thread::spawn(move || {
        const PROBE_INTERVAL: Duration = Duration::from_secs(2);
        const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
        const UNHEALTHY_THRESHOLD: u32 = 2;
        let client = match reqwest::blocking::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                eprintln!("watcher[{id_owned}]: cannot build http client: {error}");
                return;
            }
        };
        loop {
            thread::sleep(PROBE_INTERVAL);
            let Some(snapshot) = host::read_host_record(&id_owned).ok().flatten() else {
                // Record gone — the user deleted the container.
                return;
            };
            // Step 1: liveness probe via PID file (cross-platform via
            // `kill -0` on unix; Windows equivalent below).
            if !pid_alive(snapshot.host_pid) {
                let _ = host::compare_and_swap_host_record(&id_owned, &snapshot, |on_disk| {
                    let mut next = on_disk.clone();
                    next.state = HostState::Crashed;
                    next.exit_status = Some(-1);
                    next.exit_signal = None;
                    next
                });
                return;
            }
            // Step 2: HTTP probe.
            let healthy = client
                .get(&url)
                .send()
                .map(|response| response.status().is_success())
                .unwrap_or(false);
            let _ = host::compare_and_swap_host_record(&id_owned, &snapshot, |on_disk| {
                let mut next = on_disk.clone();
                next.probe_count = on_disk.probe_count.saturating_add(1);
                if healthy {
                    next.last_seen = box_foundation::now_seconds();
                    next.unhealthy_count = 0;
                    if matches!(next.state, HostState::Starting | HostState::Ready) {
                        next.state = HostState::Running;
                    }
                } else {
                    next.unhealthy_count = on_disk.unhealthy_count.saturating_add(1);
                    if next.unhealthy_count >= UNHEALTHY_THRESHOLD
                        && matches!(next.state, HostState::Starting | HostState::Ready | HostState::Running)
                    {
                        next.state = HostState::Crashed;
                    }
                }
                next
            });
            // Exit if we just marked the host Crashed — keep loop
            // tight; nothing more to do until the user restarts.
            if let Ok(Some(latest)) = host::read_host_record(&id_owned) {
                if matches!(latest.state, HostState::Crashed | HostState::Stopped) {
                    return;
                }
            }
        }
    });
}

/// Cross-platform PID liveness probe with a distinguishing error
/// channel. Unix uses `kill -0` + stderr parsing (the shell prints
/// "kill: <pid>: No such process" on ESRCH); Windows returns ACCESS_DENIED
/// from `OpenProcess` for foreign PIDs.
///
/// Linux/macOS refinement: a PID that exists but is in the Z (zombie)
/// state is treated as dead — the parent never `wait()`ed for it, so
/// no live work is happening. ESRCH from `kill -0` only catches
/// fully-reaped entries.
fn pid_alive(pid: u32) -> bool {
    matches!(probe_pid(pid), PidProbe::Alive)
}

/// Poll for the `tsx` symlink under `<source>/node_modules/` to resolve
/// successfully. pnpm install lays down `.pnpm/tsx@<ver>/node_modules/tsx`
/// before linking the top-level `node_modules/tsx` symlink, so once
/// `std::fs::metadata` succeeds on that symlink target the rest of the
/// store is also ready for the ESM resolver. Times out with the supplied
/// deadline so the caller can report a clean error rather than letting
/// node spew `ERR_MODULE_NOT_FOUND`.
fn wait_for_pnpm_links(source: &Path, deadline: Duration) -> Result<(), String> {
    let marker = source.join("node_modules").join("tsx");
    let probe = || {
        // `metadata` follows symlinks; a dangling link returns Err — that's
        // exactly what we want to wait out.
        std::fs::metadata(&marker).map(|_| ()).map_err(|error| error.to_string())
    };
    if probe().is_ok() {
        return Ok(());
    }
    let start = Instant::now();
    while start.elapsed() < deadline {
        if probe().is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "pnpm store link is missing at {} after {:?}",
        marker.display(),
        deadline
    ))
}

/// Scan every persisted `host.json` and reconcile it against the live
/// process table. Called once at daemon startup so that a previous
/// daemon's death doesn't leave stale "running" records behind.
///
///   - `Crashed` / `Stopped` → leave alone; the user is expected to
///     clear them via `dshbox container start` or `rm`.
///   - `Starting` / `Ready` / `Running` with PID dead → remove the
///     record so `start` can rebuild it.
///   - `Starting` / `Ready` / `Running` with PID alive but EPERM'd
///     (PID was recycled by an unrelated process) → mark `Orphaned`.
///   - `Orphaned` is left as-is; user must restart to clear.
pub(crate) fn reconcile_orphan_containers() {
    let records = match host::list_all_host_records() {
        Ok(r) => r,
        Err(error) => {
            eprintln!("reconcile: cannot list host.json files: {error}");
            return;
        }
    };
    for record in records {
        if !matches!(
            record.state,
            HostState::Starting | HostState::Ready | HostState::Running
        ) {
            continue;
        }
        match probe_pid(record.host_pid) {
            PidProbe::Alive => {
                // PID exists; the recorded host may still be running
                // on another machine or after a daemon crash. Trust the
                // watcher to decide its fate.
            }
            PidProbe::Esrch => {
                eprintln!(
                    "reconcile: {} host PID {} is gone; dropping stale record",
                    record.id, record.host_pid
                );
                host::remove_host_record(&record.id);
                // Also remove the legacy host.pid file so callers that
                // still consult it don't trip over a phantom running
                // container.
                if let Ok(config) = read_config() {
                if let Some(root) = config.runtime_directory {
                    let pid_path = container_directory(&root, &record.id)
                        .join("state")
                        .join("host.pid");
                    let _ = fs::remove_file(pid_path);
                }
            }
            }
            PidProbe::Eperm => {
                eprintln!(
                    "reconcile: {} host PID {} exists but is not ours; flagging orphaned",
                    record.id, record.host_pid
                );
                let _ = host::compare_and_swap_host_record(
                    &record.id,
                    &record,
                    |on_disk| {
                        let mut next = on_disk.clone();
                        next.state = HostState::Orphaned;
                        next
                    },
                );
            }
        }
    }
}

enum PidProbe {
    Alive,
    Esrch,
    Eperm,
}

/// Cross-platform PID existence probe with a distinguishing error
/// channel. Unix uses `kill -0` + stderr parsing (the shell prints
/// "kill: <pid>: No such process" on ESRCH); Windows returns ACCESS_DENIED
/// from `OpenProcess` for foreign PIDs.
fn probe_pid(pid: u32) -> PidProbe {
    #[cfg(unix)]
    {
        // Fast path: /proc/<pid> missing ⇒ fully gone. Reading
        // /proc avoids locale-sensitive shell error parsing.
        if std::fs::read_to_string(format!("/proc/{pid}/status")).is_err() {
            return PidProbe::Esrch;
        }
        // Distinguish zombie (R → Z) from alive. A zombie PID
        // technically responds to kill -0 with success, so the
        // status check alone misreports it as alive.
        if let Ok(status_text) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status_text.lines() {
                if let Some(rest) = line.strip_prefix("State:") {
                    if rest.trim().starts_with('Z') {
                        return PidProbe::Esrch;
                    }
                    break;
                }
            }
        }
        let output = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .output();
        match output {
            Ok(result) if result.status.success() => PidProbe::Alive,
            Ok(result) => {
                let stderr = String::from_utf8_lossy(&result.stderr);
                // kill(1) prints one of these regardless of LANG — the
                // translation happens in the shell wrapper, not in
                // kill itself.
                if stderr.contains("No such process")
                    || stderr.contains("ESRCH")
                    || stderr.contains("does not exist")
                {
                    PidProbe::Esrch
                } else {
                    PidProbe::Eperm
                }
            }
            Err(_) => PidProbe::Esrch,
        }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn OpenProcess(
                access: u32,
                inherit: i32,
                pid: u32,
            ) -> *mut core::ffi::c_void;
            fn GetLastError() -> u32;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const ERROR_ACCESS_DENIED: u32 = 5;
        let handle = unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
        };
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
            PidProbe::Alive
        } else {
            let error = unsafe { GetLastError() };
            if error == ERROR_ACCESS_DENIED {
                PidProbe::Eperm
            } else {
                PidProbe::Esrch
            }
        }
    }
}
