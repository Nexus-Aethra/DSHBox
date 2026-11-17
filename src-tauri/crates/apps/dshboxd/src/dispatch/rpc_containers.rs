//! Container RPCs: list, start, stop, rebuild, restart, delete, describe, and
//! the URL probe.
//!
//! Every one of these either reads container state or enqueues a scheduler
//! task through the primitives in mod.rs. Nothing here knows about templates
//! or the extension repository.

use super::*;

pub(super) fn get_info() -> Result<Value, String> {
    let config = read_config()?;
    let mut info = json!({
        // The application version this install was released as. The crate has
        // its own 0.1.0 and reporting that here is what made the updater treat
        // an up-to-date install as behind.
        "version": crate::updates::APP_VERSION,
        "buildStamp": env!("DSHBOX_BUILD_STAMP"),
        "pid": std::process::id(),
        "runtimeDirectory": config.runtime_directory,
        "githubMirror": config.github_mirror,
        "npmRegistry": config.npm_registry,
    });
    if let Some(root) = config.runtime_directory.as_ref() {
        let containers = box_containers::scan_containers(root)
            .map(|items| items.len())
            .unwrap_or(0);
        info["containers"] = json!(containers);
        info["repositoryEntries"] = json!(scan_repository(std::path::Path::new(root)).len());
        info["bundles"] = json!(read_bundles(std::path::Path::new(root)).len());
        info["dshVersions"] = json!(installed_versions(root)
            .map(|items| items.len())
            .unwrap_or(0));
    }
    if let Ok(runtime) = bundled_runtime() {
        info["runtime"] = json!({
            "node": runtime.node_version,
            "npm": runtime.npm_version,
            "pnpm": runtime.pnpm_version,
        });
    }
    Ok(info)
}

pub(crate) fn list_containers(state: &DaemonState) -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut containers = box_containers::scan_containers(&root)?
        .into_values()
        .collect::<Vec<_>>();
    let running = state_running_containers(&state.containers);
    for container in &mut containers {
        // The daemon registry contains only children started by this daemon
        // process.  A daemon restart deliberately loses those Child handles,
        // but a persisted host PID can still be healthy (especially on
        // Windows, where the desktop daemon is commonly restarted during an
        // update).  Keep the CLI and UI aligned with that durable signal.
        container.status =
            if running.contains(&container.id) || box_containers::is_host_alive(container) {
                "running".to_owned()
            } else if matches!(
                host::read_host_record(&container.id)
                    .ok()
                    .flatten()
                    .map(|r| r.state),
                Some(HostState::Corrupted)
            ) {
                "corrupted".to_owned()
            } else {
                "stopped".to_owned()
            };
    }
    Ok(json!(containers))
}


pub(super) fn stop_container_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = crate::resolve::resolve_container_key(state, request["id"].as_str().unwrap_or(""))?;
    stop_dsh_container(&id, &state.containers)?;
    Ok(json!({ "id": id, "stopped": true }))
}

pub(super) fn container_url_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // The running-host registry and the host record are both keyed by the
    // canonical id, so an alias asked for here is not a lookup that fails --
    // it is a lookup that answers "container is not running" for a container
    // that plainly is. The reply carries the resolved id for the same reason
    // `box_navigate` can then use it without asking twice.
    let id = crate::resolve::resolve_container_key(state, request["id"].as_str().unwrap_or(""))?;
    container_url(state, &id).map(|url| json!({ "id": id, "url": url }))
}

/// The URL a caller may actually open, or why there is not one yet.
///
/// A `Starting` record with no authenticated URL is a host that has not
/// finished booting. An old runtime that serves the bare URL is a different
/// thing, and it is never `Starting` without one, so the state tells the two
/// apart -- not the absence of the field.
///
/// Handing out the bare URL during that window is what makes this a trap: DSH
/// 0.1.2+ answers every tokenless request with 401, so the webview and the
/// headless browser both land on "authentication required". That reads like a
/// permission problem rather than "not yet", and nothing in the reply tells
/// the caller to wait, so an agent opens it once, sees a wall, and concludes
/// the container is broken.
pub(super) fn usable_url(record: &host::ContainerHostRecord) -> Result<String, String> {
    match (&record.authenticated_url, record.state) {
        (Some(url), _) => Ok(url.clone()),
        (None, host::HostState::Starting) => Err(format!(
            "container {} is still starting; its URL cannot be opened until the host announces one (retry in a moment)",
            record.id
        )),
        (None, _) => Ok(record.host_url.clone()),
    }
}
/// Resolve a running container's authenticated loopback URL.
///
/// Whether a container still has a running host, read without daemon state.
///
/// `container_url` needs the in-memory registry because a live host is served
/// from there. Liveness cleanup does not: it walks the session map on a call
/// that may have no registry entry for the container at all, and taking a
/// `DaemonState` would force that call to borrow state it otherwise never
/// touches. The durable host record plus a pid check answers the same question
/// for every container that is not being started this very instant.
pub(crate) fn container_url_probe(id: &str) -> Result<String, String> {
    let record =
        host::read_host_record(id).map_err(|error| format!("cannot read host record: {error}"))?;
    match record.filter(|record| {
        matches!(
            record.state,
            HostState::Starting | HostState::Ready | HostState::Running
        ) && box_containers::is_host_pid_alive(record.host_pid)
    }) {
        Some(record) => usable_url(&record),
        None => Err(format!("container is not running: {id}")),
    }
}

/// DSH 0.1.2+ answers every tokenless request with 401, so callers that open
/// a browser at this URL need the per-launch capability form, not the bare
/// host URL. Shared by the webview (`container_url_rpc`) and the headless
/// debug browser, which must both present a usable URL.
pub(crate) fn container_url(state: &DaemonState, id: &str) -> Result<String, String> {
    let id = id.to_owned();
    let running = state
        .containers
        .running
        .lock()
        .map_err(|_| "container manager lock failed".to_owned())?;
    if let Some(host) = running.get(&id) {
        // DSH 0.1.2+ rejects tokenless requests with 401, so prefer the
        // authenticated URL when the start path parsed one.
        if let Some(url) = &host.authenticated_url {
            return Ok(url.clone());
        }
        // No token in the in-memory entry is ambiguous -- the host has not
        // announced one yet, or the runtime does not use them -- and only the
        // record can tell those apart, because it carries the state. It is
        // written before the readiness probe begins, so it covers the whole
        // window this registry entry lives in.
    }
    drop(running);
    let record =
        host::read_host_record(&id).map_err(|error| format!("cannot read host record: {error}"))?;
    match record.filter(|record| {
        matches!(
            record.state,
            HostState::Starting | HostState::Ready | HostState::Running
        ) && box_containers::is_host_pid_alive(record.host_pid)
    }) {
        Some(record) => usable_url(&record),
        None => Err(format!("container is not running: {id}")),
    }
}


pub(super) fn enqueue_task(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let kind = request["kind"].as_str().unwrap_or("");
    let resource_keys: Vec<String> = request["resource_keys"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    // Design section 3.5: the generic enqueue route used to accept params of
    // any size. A task record is not a payload channel, and a whole-scope
    // upsert of a multi-MB row is exactly what section 6 rules out.
    let params_size = serde_json::to_vec(&params)
        .map(|bytes| bytes.len())
        .map_err(|error| format!("params must be JSON: {error}"))?;
    if params_size > box_scheduler::MAX_PARAMS_BYTES {
        return Err(format!(
            "params exceed {} bytes: {params_size}",
            box_scheduler::MAX_PARAMS_BYTES
        ));
    }
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    state
        .manager
        .enqueue(&paths, kind, resource_keys, params)
        .map(HandlerResult::Async)
        .map_err(|e| e.to_string())
}


pub(super) fn create_container_rpc(request: &Value) -> Result<Value, String> {
    let name = request["name"].as_str().unwrap_or("").to_owned();
    let version = request["version"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    create_dsh_container_sync(&name, &version, &profile).map(|container| json!(container))
}


pub(super) fn enqueue_container_start(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let raw = request["id"].as_str().unwrap_or("").to_owned();
    if raw.is_empty() {
        return Err("expected a container id".to_owned());
    }
    // Resolved before the task is queued, because the scope below is the lock
    // that serialises this container's tasks. An alias would enqueue a task
    // keyed `container:alpha` beside one keyed `container:container-1730...`
    // and let both stop, start and rewrite the same host at once.
    let id = crate::resolve::resolve_container_key(state, &raw)?;
    let containers = state.containers.clone();
    let params = json!({ "id": id.clone() });
    enqueue_task_worker(
        state,
        "container-start",
        vec![format!("container:{id}")],
        params,
        move |task| {
            task.update("Starting DSH host", 10);
            task.check_cancelled()?;
            start_dsh_container_inner(&id, &containers.running, Some(task)).map(|_| ())
        },
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_container_stop(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let raw = request["id"].as_str().unwrap_or("").to_owned();
    if raw.is_empty() {
        return Err("expected a container id".to_owned());
    }
    // Resolved before the task is queued, because the scope below is the lock
    // that serialises this container's tasks. An alias would enqueue a task
    // keyed `container:alpha` beside one keyed `container:container-1730...`
    // and let both stop, start and rewrite the same host at once.
    let id = crate::resolve::resolve_container_key(state, &raw)?;
    let containers = state.containers.clone();
    let params = json!({ "id": id.clone() });
    enqueue_task_worker(
        state,
        "container-stop",
        vec![format!("container:{id}")],
        params,
        move |task| {
            task.update("Stopping DSH host", 30);
            task.check_cancelled()?;
            stop_dsh_container(&id, &containers)
        },
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_container_rebuild(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let raw = request["id"].as_str().unwrap_or("").to_owned();
    if raw.is_empty() {
        return Err("expected a container id".to_owned());
    }
    // Resolved before the task is queued, because the scope below is the lock
    // that serialises this container's tasks. An alias would enqueue a task
    // keyed `container:alpha` beside one keyed `container:container-1730...`
    // and let both stop, start and rewrite the same host at once.
    let id = crate::resolve::resolve_container_key(state, &raw)?;
    let containers = state.containers.clone();
    let params = json!({ "id": id.clone() });
    enqueue_task_worker(
        state,
        "container-rebuild",
        vec![format!("container:{id}")],
        params,
        move |task| {
            task.update("Rebuilding DSH runtime", 10);
            task.check_cancelled()?;
            rebuild_dsh_container_with_task(id, &containers, Some(task))
        },
    )
    .map(HandlerResult::Async)
}

/// Manual restart of a container that has been marked `Crashed` (or
/// `Stopped`) by the health watcher. Unlike `rebuild`, this keeps the
/// existing build artifacts and just re-spawns the host. The host
/// watcher detects the new PID automatically because
/// `start_dsh_container_inner` always allocates a fresh record before
/// spawning the readiness probe.
pub(super) fn enqueue_container_restart(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let raw = request["id"].as_str().unwrap_or("").to_owned();
    if raw.is_empty() {
        return Err("expected a container id".to_owned());
    }
    // Resolved before the task is queued, because the scope below is the lock
    // that serialises this container's tasks. An alias would enqueue a task
    // keyed `container:alpha` beside one keyed `container:container-1730...`
    // and let both stop, start and rewrite the same host at once.
    let id = crate::resolve::resolve_container_key(state, &raw)?;
    let containers = state.containers.clone();
    let params = json!({ "id": id.clone() });
    enqueue_task_worker(
        state,
        "container-restart",
        vec![format!("container:{id}")],
        params,
        move |task| {
            task.update("Stopping old host", 10);
            task.check_cancelled()?;
            // Best-effort stop; ignore "not running" errors so a
            // restart can be issued against a Crashed record.
            let _ = stop_dsh_container(&id, containers.as_ref());
            // Drop the tombstone so the next start treats this as a
            // fresh launch instead of inheriting a stale generation.
            host::remove_host_record(&id);
            task.update("Starting DSH host", 30);
            task.check_cancelled()?;
            start_dsh_container_inner(&id, &containers.running, Some(task)).map(|_| ())
        },
    )
    .map(HandlerResult::Async)
}


/// Whether a parsed container.json asks for a WSL2 VM. Kept as a pure
/// function so the delete path and its tests cannot drift: RuntimeKind::parse
/// accepts several spellings (`vm`, `vm:wsl2`, `vm-wsl2`, `VM:WSL2`, …)
/// and a literal string match would leave an orphan distro behind.
#[cfg(any(target_os = "windows", test))]
pub(super) fn record_requests_wsl_vm(value: &serde_json::Value) -> bool {
    match crate::runtime::RuntimeKind::from_record(value) {
        // `vm` means the platform default, which on Windows is wsl2.
        Ok(crate::runtime::RuntimeKind::Vm(None)) => true,
        Ok(crate::runtime::RuntimeKind::Vm(Some(backend))) => {
            backend.eq_ignore_ascii_case("wsl2")
        }
        _ => false,
    }
}


pub(super) fn delete_container_rpc(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    stop_dsh_container(&id, &state.containers)?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Only a vm:wsl2 container ever leaves a registered WSL distro behind.
    // Read the record before deleting its directory, gate on both platform
    // and declared runtime. A failed unregister must fail the delete: the
    // container directory is still intact here, so the user can retry,
    // whereas deleting it first would orphan the distro and its vhdx.
    #[cfg(target_os = "windows")]
    {
        let record = Path::new(&root)
            .join("instances")
            .join(&id)
            .join("container.json");
        let is_wsl_vm = std::fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| record_requests_wsl_vm(&value));
        if is_wsl_vm {
            crate::vm::wsl::unregister_container(&id).map_err(|error| {
                format!("could not unregister WSL distro for {id}: {error}")
            })?;
        }
    }
    // Reconcile before mutating so a stale file does not cause the
    // owner map to drift from the canonical sources we're about to
    // release.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
    let directory = Path::new(&root).join("instances").join(&id);
    if !directory.is_dir() {
        return Err(format!("container not found: {id}"));
    }
    std::fs::remove_dir_all(&directory)
        .map_err(|error| format!("cannot remove container: {error}"))?;
    // Data payloads are temporary storage that follows container lifecycles
    // (no reference counting): garbage-collect the store now that this
    // container's usage records are gone.
    let _ = crate::data::prune_orphaned_data();
    Ok(HandlerResult::Sync(json!({ "id": id, "deleted": true })))
}


/// Compose the full container description used by `dshbox container describe`.
/// Reuses the on-disk `DshContainer` summary as the base, then enriches it
/// with live runtime signals (URL, host PID) and the same extensions scan
/// the desktop details panel uses (`scan_container_extensions`). The result
/// is serialised through `box_api::ContainerDescription` so the wire shape
/// is identical for every consumer (CLI text, CLI `--json`, future UI).
pub(super) fn describe_container_rpc(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    // A describe is a read, and the container in it has to be one the caller
    // can name. The running-host lookup below is keyed by this id, so resolving
    // here is also what keeps a description from reporting "stopped" for a
    // container that is running under its canonical id. No charset gate here:
    // names are only length-checked at creation, and the resolver applies its
    // join gate exactly where a path would be built.
    let id = crate::resolve::resolve_container_key(state, &id)?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(&id)
        .ok_or_else(|| format!("container not found: {id}"))?;
    // Live signals: the URL is only known while the host process is in our
    // running registry; the PID file + `is_host_alive` probe decide whether
    // the host is actually still alive (a crash leaves the registry stale
    // until the next user action). The PID file is the source of truth for
    // `status` — the URL is informational and may be absent for short-lived
    // races between the daemon adding the registry entry and writing the
    // PID file.
    let url = state
        .containers
        .running
        .lock()
        .map_err(|_| "container manager lock failed".to_owned())?
        .get(&id)
        // DSH 0.1.2+ rejects tokenless requests with 401, so prefer the
        // authenticated URL when the start path parsed one.
        .map(|host| {
            host.authenticated_url
                .clone()
                .unwrap_or_else(|| host.url.clone())
        });
    let host_pid = read_live_host_pid(&container);
    let status = if host_pid.is_some() {
        "running"
    } else {
        "stopped"
    };
    let extensions = scan_container_extensions(&container);
    let description = ContainerDescription {
        id: container.id.clone(),
        name: container.name.clone(),
        version: container.version.clone(),
        profile: container.profile.clone(),
        template: container.template.clone(),
        directory: container.directory.clone(),
        status: status.to_owned(),
        // Normalised through the resolver's parser so a record carrying a
        // malformed value still answers "host" here — the read verb must not
        // fail; the start verb is where a bad runtime is an error.
        runtime: container
            .runtime
            .as_deref()
            .and_then(|text| crate::runtime::RuntimeKind::parse(text).ok())
            .map(|kind| kind.field())
            .unwrap_or_else(|| "host".to_owned()),
        url,
        host_pid,
        extensions: serde_json::to_value(&extensions)
            .map_err(|error| format!("cannot serialize extensions: {error}"))?,
    };
    let value = serde_json::to_value(&description)
        .map_err(|error| format!("cannot serialize description: {error}"))?;
    Ok(HandlerResult::Sync(value))
}

/// Read the PID file and confirm the host process is still alive (Unix
/// `kill -0`; Windows `tasklist`). Mirrors `is_host_alive` from
/// `box_containers` but returns the PID rather than a boolean — callers
/// that only need a flag still go through the box-containers helper.
pub(super) fn read_live_host_pid(container: &box_containers::DshContainer) -> Option<u32> {
    let pid_path = box_containers::host_pid_path(container);
    let pid_text = std::fs::read_to_string(&pid_path).ok()?;
    let pid = pid_text.trim().parse::<u32>().ok()?;
    if box_containers::is_host_alive(container) {
        Some(pid)
    } else {
        None
    }
}


/// Safe relative path check for workspace extension imports (mirrors the
/// desktop's guard: no absolute paths, no parent/root components).
pub(super) fn is_safe_workspace_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !Path::new(value).is_absolute()
        && !Path::new(value).components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}
