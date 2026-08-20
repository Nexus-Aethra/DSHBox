//! Container lifecycle for the daemon: start the DSH host process for a
//! container, wait for it to become ready, and stop it. Mirrors the
//! desktop's `lifecycle.rs` without any Tauri dependency; the daemon owns
//! the child processes and the running registry.

use crate::containers::{
    ensure_container_workspace, repair_known_profile_template,
    write_dshbox_context_snapshot,
};
use crate::state::{ContainerManager, ManagedHost};
use crate::toolchains::{resolve_toolchain, run_logged};
use box_containers::container_directory;
use box_dsh_context::PLUGIN_ID;
use box_foundation::{is_safe_identifier, read_config};
use box_runtime::process::{self, ExecutionKind, ProcessSpec, TrackedChild};
use box_scheduler::TaskContext;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use crate::host::{self, HostState};

fn dsh_dependencies_ready(source: &Path) -> bool {
    // `pnpm install` can leave the root node_modules directory behind when
    // Windows fails during junction creation/final validation. The frontend
    // build imports tsx directly, so this package manifest is the smallest
    // reliable marker that the workspace is actually ready to build.
    source
        .join("node_modules")
        .join("tsx")
        .join("package.json")
        .is_file()
}

const HOST_BIND_ATTEMPTS: u32 = 3;
const HOST_READY_PROBES: usize = 240;

fn allocate_loopback_port() -> Result<u16, String> {
    // This reservation only identifies a currently usable port; DSH owns the
    // eventual listener, so the socket must be released before host spawn.
    // Keep the allocation immediately adjacent to that spawn to minimise the
    // unavoidable hand-off window.
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot allocate loopback port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| error.to_string())
}

fn is_transient_loopback_bind_failure(output: &[u8]) -> bool {
    let output = String::from_utf8_lossy(output);
    output.contains("listen EACCES")
        || output.contains("listen EADDRINUSE")
        || output.contains("permission denied 127.0.0.1")
        || output.contains("address already in use 127.0.0.1")
}

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
    // Sealed recipes retain `sealedTemplate`; direct official-template runs
    // are materialised from a prepared base and rely on the local manifest.
    // Both forms are self-contained once preparation has completed.
    if !directory.join("manifest.json").is_file() {
        return Err("container is missing its template manifest".to_owned());
    }
    let _version = value["version"]
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
    let source = directory.join("harness");
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
    if let Some(task) = task {
        task.check_cancelled()?;
    }
    let patch = directory.join("box-web.patch.yml");
    let log_path = directory.join("logs").join("host.log");
    let _ = fs::File::create(&log_path);
    if !dsh_dependencies_ready(&source)
        || !source.join("apps/web/dist/index.html").is_file()
    {
        return Err("sealed container is missing prepared DSH dependencies or frontend build".to_owned());
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
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
        let port = allocate_loopback_port()?;
        let url = format!("http://127.0.0.1:{port}");
        fs::write(
            &patch,
            format!("- id: webserver\n  config:\n    host: 127.0.0.1\n    port: {port}\n"),
        )
        .map_err(|error| format!("cannot write web patch: {error}"))?;
        let log_offset = fs::metadata(&log_path).map(|metadata| metadata.len()).unwrap_or(0);
        if let Some(task) = task {
            task.log(&format!("starting DSH host on {url}"));
        }
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
        let ready = (0..HOST_READY_PROBES).any(|attempt| {
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
                if attempt % 20 == 0 {
                    task.log(&format!(
                        "waiting for DSH host ({}/60s)",
                        attempt / 4
                    ));
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
        let host_output = fs::read(&log_path)
            .ok()
            .and_then(|output| output.get(log_offset as usize..).map(|tail| tail.to_vec()))
            .unwrap_or_default();
        if is_transient_loopback_bind_failure(&host_output) && attempt < HOST_BIND_ATTEMPTS {
            if let Some(task) = task {
                task.log(&format!(
                    "DSH host could not bind {url}; selecting a new loopback port (attempt {}/{HOST_BIND_ATTEMPTS})",
                    attempt + 1
                ));
            }
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

/// A sealed container already contains a prepared frontend. Rebuild is kept
/// as a UI-compatible recovery action, but now means a clean host restart;
/// mutating the container with `pnpm install` or `pnpm build` would break the
/// sealed-template contract.
pub(crate) fn rebuild_dsh_container_with_task(
    id: String,
    manager: &ContainerManager,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    if let Some(task) = task {
        task.update("Stopping DSH host", 20);
    }
    stop_dsh_container(&id, manager)?;
    if let Some(task) = task {
        task.update("Validating sealed container", 55);
        task.check_cancelled()?;
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
    ensure_vendored_plugin_copied(&vendored, &profile_node_modules)
}

/// Idempotent exposure of the vendored bundle under a profile's
/// `node_modules`. Always a physical copy (`cp -rL` semantics) so the
/// container profile never references the runtime tree via symlinks or
/// junctions — those are incompatible with Windows + pnpm + AV scenarios
/// and have no upside now that cross-container dedup is opt-in per build.
/// Freshness is detected by mtime + sha256(`package.json`) so an unchanged
/// vendored tree does not pay for a full re-copy on every container start.
fn ensure_vendored_plugin_copied(
    vendored: &Path,
    profile_node_modules: &Path,
) -> Result<(), String> {
    let scoped = profile_node_modules.join("@deepseek-ai");
    fs::create_dir_all(&scoped)
        .map_err(|error| format!("cannot create {}: {error}", scoped.display()))?;
    let target = scoped.join(PLUGIN_ID);
    // Already a plain directory whose contents match the source — leave it.
    if target.is_dir()
        && !fs::symlink_metadata(&target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        && packages_match(vendored, &target)?
    {
        return Ok(());
    }
    // Otherwise it is a stale symlink, an unexpected file, or a divergent
    // copy — clear it before re-copying. `symlink_metadata` is used so we
    // do not follow the link itself when deciding what to remove.
    if let Ok(metadata) = fs::symlink_metadata(&target) {
        if metadata.file_type().is_symlink() {
            fs::remove_file(&target)
                .map_err(|error| format!("cannot replace stale plugin link: {error}"))?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(&target)
                .map_err(|error| format!("cannot replace stale plugin dir: {error}"))?;
        } else {
            fs::remove_file(&target)
                .map_err(|error| format!("cannot replace stale plugin file: {error}"))?;
        }
    }
    copy_tree_following(vendored, &target)
        .map_err(|error| format!("cannot copy vendored plugin into profile: {error}"))
}

/// True when the two plugin trees share the same `package.json` bytes.
/// (The vendored tree may carry an outdated `node_modules/`; the simple
/// equality check on `package.json` is enough — that file is rewritten
/// whenever the plugin is re-extracted, which is the only thing the user
/// can meaningfully change in the vendored tree.)
fn packages_match(first: &Path, second: &Path) -> Result<bool, String> {
    let a = fs::read(first.join("package.json"))
        .map_err(|error| format!("cannot read {}/package.json: {error}", first.display()))?;
    let b = fs::read(second.join("package.json"))
        .map_err(|error| format!("cannot read {}/package.json: {error}", second.display()))?;
    Ok(a == b)
}

/// Recursive directory copy with `cp -rL` semantics: any symlinks
/// encountered *inside* the source tree are dereferenced and their target
/// contents materialised, never re-exported as a link. This keeps each
/// container profile self-contained and prevents the Windows pnpm +
/// AV + symlink interactions we have hit in the past.
pub(crate) fn copy_tree_following(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    let mut ancestors = HashSet::new();
    copy_tree_following_inner(source, destination, &mut ancestors)
}

fn copy_tree_following_inner(
    source: &Path,
    destination: &Path,
    ancestors: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    // Package-manager trees are graphs, not necessarily trees: pnpm can
    // create a package-local junction that ultimately points back to an
    // ancestor package. `cp -rL` semantics must reject that impossible-to-
    // materialize cycle instead of recursing until dshboxd stack-overflows.
    let identity = fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    if !ancestors.insert(identity.clone()) {
        // The source is a package-manager graph. A reparse point back to an
        // ancestor cannot be represented by a finite physical tree; omitting
        // this back-edge is safe because Node resolves that dependency from
        // the already materialized ancestor `node_modules` directory.
        return Ok(());
    }
    let result = (|| {
    fs::create_dir_all(destination)
        .map_err(|error| copy_tree_error(error, "create directory", source, destination))?;
    for entry in fs::read_dir(source)
        .map_err(|error| copy_tree_error(error, "read directory", source, destination))?
    {
        let entry = entry.map_err(|error| copy_tree_error(error, "read directory entry", source, destination))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        // `symlink_metadata` does not follow symlinks, so a symlink to a
        // directory here still reports `is_symlink()` plus `is_dir()`. We
        // recurse only on plain directories and treat everything else as
        // a leaf whose target we materialise.
        let metadata = fs::symlink_metadata(&from)
            .map_err(|error| copy_tree_error(error, "read metadata", &from, &to))?;
        if metadata.file_type().is_symlink() {
            // Resolve the link, then copy whatever it points at. If the
            // target is a directory we recurse; otherwise we hit the file
            // branch below.
            let resolved = fs::canonicalize(&from).unwrap_or_else(|_| from.clone());
            // `canonicalize` resolves a Windows pnpm junction, but
            // `symlink_metadata` on the resolved path can still report a
            // reparse-point file type. Use `metadata` here deliberately: we
            // need the target's real directory/file kind before deciding
            // between recursive copy and `fs::copy`.
            let resolved_meta = fs::metadata(&resolved)
                .map_err(|error| copy_tree_error(error, "read dereferenced metadata", &resolved, &to))?;
            if resolved_meta.is_dir() {
                copy_tree_following_inner(&resolved, &to, ancestors)
                    .map_err(|error| copy_tree_error(error, "copy dereferenced directory", &resolved, &to))?;
            } else {
                fs::copy(&resolved, &to)
                    .map_err(|error| copy_tree_error(error, "copy dereferenced file", &resolved, &to))?;
            }
        } else if metadata.is_dir() {
            copy_tree_following_inner(&from, &to, ancestors)
                .map_err(|error| copy_tree_error(error, "copy directory", &from, &to))?;
        } else if metadata.is_file() {
            fs::copy(&from, &to)
                .map_err(|error| copy_tree_error(error, "copy file", &from, &to))?;
        }
    }
    Ok(())
    })();
    ancestors.remove(&identity);
    result
}

fn copy_tree_error(
    error: std::io::Error,
    action: &str,
    source: &Path,
    destination: &Path,
) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!("{action} {} -> {}: {error}", source.display(), destination.display()),
    )
}

/// Replace a destination tree with a dereferenced physical copy.  Package
/// managers can replace a copied package with links during install, so callers
/// that promise a container-owned plugin payload must use this after install
/// as well as before it.
pub(crate) fn replace_tree_following(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(destination)?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(destination)?;
        }
    }
    copy_tree_following(source, destination)
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

#[cfg(test)]
mod dependency_ready_tests {
    use super::*;
    use std::fs;

    #[test]
    fn requires_the_tsx_manifest_not_just_node_modules() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-dependency-ready-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(root.join("node_modules")).unwrap();
        assert!(!dsh_dependencies_ready(&root));

        fs::create_dir_all(root.join("node_modules/tsx")).unwrap();
        fs::write(root.join("node_modules/tsx/package.json"), "{}").unwrap();
        assert!(dsh_dependencies_ready(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recognises_retryable_loopback_bind_errors() {
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EACCES: permission denied 127.0.0.1:54329"
        ));
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EADDRINUSE: address already in use 127.0.0.1:54329"
        ));
        assert!(!is_transient_loopback_bind_failure(
            b"plugin tree failed to load"
        ));
    }
}

#[cfg(test)]
mod copy_tree_following_tests {
    use super::*;
    use std::fs;

    fn sandbox(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-ctf-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_plain_tree_recursively() {
        let src = sandbox("plain");
        let dst = sandbox("plain-dst");
        fs::write(src.join("hello.txt"), "hi").unwrap();
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested/inner.txt"), "deep").unwrap();

        copy_tree_following(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "hi");
        assert!(dst.join("nested/inner.txt").is_file());
        assert_eq!(fs::read_to_string(dst.join("nested/inner.txt")).unwrap(), "deep");
    }

    #[test]
    #[cfg(unix)]
    fn dereferences_internal_symlinks() {
        use std::os::unix::fs::symlink;
        let src = sandbox("sym");
        let dst = sandbox("sym-dst");
        fs::write(src.join("target.txt"), "content").unwrap();
        symlink("target.txt", src.join("link")).unwrap();

        copy_tree_following(&src, &dst).unwrap();
        // The resulting dir must contain a plain file named `link` whose
        // contents match the symlink target — not another symlink.
        let metadata = fs::symlink_metadata(dst.join("link")).unwrap();
        assert!(!metadata.file_type().is_symlink(), "expected plain file");
        assert_eq!(fs::read_to_string(dst.join("link")).unwrap(), "content");
    }

    #[test]
    #[cfg(unix)]
    fn terminates_when_a_link_points_to_an_ancestor() {
        use std::os::unix::fs::symlink;
        let src = sandbox("cycle");
        let dst = sandbox("cycle-dst");
        fs::write(src.join("real.txt"), "content").unwrap();
        symlink(".", src.join("cycle")).unwrap();

        copy_tree_following(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("real.txt")).unwrap(), "content");
        assert!(!dst.join("cycle").exists());
    }

    #[test]
    fn does_not_error_on_pre_existing_unrelated_destination() {
        let src = sandbox("noop");
        let dst = sandbox("noop-dst");
        fs::write(src.join("x.txt"), "x").unwrap();
        // `create_dir_all` on a path that already exists is fine — copy
        // must not error out on its own.
        fs::create_dir_all(&dst).unwrap();
        copy_tree_following(&src, &dst).unwrap();
        assert!(dst.join("x.txt").is_file());
    }
}
