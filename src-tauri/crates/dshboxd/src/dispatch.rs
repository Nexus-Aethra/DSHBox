//! RPC dispatch: every method the daemon answers, driven by the shared
//! task queue and daemon-owned state. Clients are thin: they serialize a
//! request, read a response, and poll task status for long operations.

use crate::bundles::{
    create_extension_bundle, delete_extension_bundle, export_extension_bundle,
    import_extension_bundle, install_container_bundle,
};
use crate::containers::create_dsh_container_sync;
use crate::data::{list_data_entries, prune_orphaned_data};
use crate::extensions::{
    container_list_plugins, container_plugin_add, link_repository_extension,
    export_repository_extension, export_repository_plugin, import_into_repository,
    import_workspace_extension, install_container_extension, prune_unused_repository_extensions,
    remove_repository_extension, remove_repository_plugin,
};
use crate::image::{
    build_image_from_script, export_template, import_template, list_templates,
    materialize_template_container, prune_template_snapshots, read_template, remove_template,
    BuildImageRequest, CreateTemplateContainerRequest,
};
use crate::host::{self, HostState};
use crate::lifecycle::{
    rebuild_dsh_container_with_task, start_dsh_container_inner, stop_dsh_container,
};
use crate::state::{bundled_runtime, ContainerManager, DaemonNotifier, DaemonState};
use crate::versions::{
    catalog_names, pull_template_with_cancel, refresh_dsh_catalog, uninstall_dsh_version,
    upgrade_legacy_resources,
};
use box_api::ContainerDescription;
use box_dsh_versions::{
    installed_versions, parse_template_ref, read_built_template, read_template_index,
};
use box_extensions::{read_bundles, scan_container_extensions, scan_repository};
use box_foundation::{now_seconds, read_config, write_config};
use box_scheduler::{run_queued, TaskContext, TaskRecord};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when a client asks the daemon to stop (build-batch mismatch during
/// an upgrade); `main.rs` exits after the response is written back.
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Dispatch one request against the daemon state. Every handler returns a
/// `{"ok": bool, ...}` frame; errors never panic the connection thread.
pub(crate) fn dispatch(state: &DaemonState, request: &Value) -> Value {
    let result = match request["method"].as_str() {
        Some("ping") => Ok(json!({
            "pid": std::process::id(),
            "status": "running",
            "startedAt": now_seconds(),
            "runtime": bundled_runtime().map(|_| "ready").unwrap_or("missing"),
        })),
        Some("get_info") => get_info(),
        Some("list_containers") => list_containers(state),
        Some("list_templates") => list_templates().map(|items| json!(items)),
        Some("read_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            read_template(&name).map(|text| json!({ "name": name, "text": text }))
        }
        Some("import_template") => {
            let archive = request["archive"].as_str().unwrap_or("").to_owned();
            let name = request["name"].as_str().map(str::to_owned).filter(|value| !value.is_empty());
            import_template(&archive, name, "rename")
                .map(|name| json!({ "name": name }))
        }
        Some("export_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            let destination = request["destination"].as_str().map(str::to_owned).filter(|value| !value.is_empty());
            export_template(&name, destination).map(|path| json!({ "path": path }))
        }
        Some("remove_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            remove_template(&name).map(|_| json!({ "name": name, "removed": true }))
        }
        Some("list_bundles") => list_bundles(),
        Some("list_repository_extensions") => list_repository_extensions(),
        Some("list_repository_reference_counts") => list_repository_reference_counts_rpc(),
        Some("list_installed_dsh_versions") => list_installed_dsh_versions(),
        Some("detect_toolchains") => detect_toolchains(),
        Some("enqueue_build") => enqueue_build(state, request),
        Some("enqueue_task") => enqueue_task(state, request),
        Some("list_dsh_catalog") => catalog_names().map(|names| json!(names)),
        Some("uninstall_dsh_version") => uninstall_dsh_version_rpc(request),
        Some("remove_repository_extension") => remove_repository_extension_rpc(request),
        Some("prune_repository_extensions") => prune_unused_repository_extensions()
            .map(|removed| json!(removed)),
        Some("prune_orphaned_data") => prune_orphaned_data().map(|removed| json!(removed)),
        Some("list_data_entries") => list_data_entries().map(|entries| json!(entries)),
        Some("container_list_plugins") => {
            let id = request["containerId"].as_str().unwrap_or("").to_owned();
            let profile = request["profile"].as_str().unwrap_or("web").to_owned();
            container_list_plugins(&id, &profile).map(|plugins| json!(plugins))
        }
        Some("create_extension_bundle") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            let ids = string_array(&request["repositoryIds"]);
            create_extension_bundle(&name, &ids).map(|bundle| json!(bundle))
        }
        Some("delete_extension_bundle") => delete_extension_bundle_rpc(request),
        Some("stop_container") => stop_container_rpc(state, request),
        Some("container_url") => container_url_rpc(state, request),
        Some("save_mirror_settings") => save_mirror_settings_rpc(request),
        Some("save_runtime_directory") => save_runtime_directory_rpc(state, request),
        Some("refresh_dsh_catalog") => enqueue_dsh_catalog_refresh(state),
        Some("pull_template") => enqueue_pull_template(state, request),
        Some("import_repository_extension") => enqueue_repository_import(state, request),
        Some("export_repository_extension") => enqueue_repository_export(state, request),
        Some("container_plugin_add") => enqueue_container_plugin_add(state, request),
        Some("export_bundle") => enqueue_bundle_export(state, request),
        Some("import_bundle") => enqueue_bundle_import(state, request),
        Some("create_container_from_template") => enqueue_template_container(state, request),
        Some("read_template_list") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            read_template_list(&name)
        }
        Some("template_info") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            template_info(&name)
        }
        Some("prune_template_snapshots") => {
            prune_template_snapshots().map(|removed| json!({ "removed": removed }))
        }
        Some("create_container") => create_container_rpc(request),
        Some("enqueue_container_start") => enqueue_container_start(state, request),
        Some("enqueue_container_stop") => enqueue_container_stop(state, request),
        Some("enqueue_container_rebuild") => enqueue_container_rebuild(state, request),
        Some("enqueue_container_restart") => enqueue_container_restart(state, request),
        Some("delete_container") => delete_container_rpc(state, request),
        Some("describe_container") => describe_container_rpc(state, request),
        Some("upgrade_legacy_resources") => upgrade_legacy_resources().map(|reports| json!(reports)),
        Some("enqueue_container_extension_add") => enqueue_container_extension_add(state, request),
        Some("enqueue_workspace_extension_import") => enqueue_workspace_extension_import(state, request),
        Some("enqueue_container_extension_copy") => enqueue_container_extension_copy(state, request),
        Some("enqueue_plugin_export") => enqueue_plugin_export(state, request),
        Some("remove_repository_plugin") => remove_repository_plugin_rpc(request),
        Some("enqueue_container_bundle_install") => enqueue_container_bundle_install(state, request),
        Some("list_tasks") => match state.manager.list() {
            Ok(tasks) => Ok(json!(tasks)),
            Err(e) => Err(e.to_string()),
        },
        Some("cancel_task") => {
            let id = request["id"].as_str().unwrap_or("");
            match state.paths.read() {
                Ok(paths) => state
                    .manager
                    .request_cancel(&paths, id)
                    .map(|_| json!({"id": id, "cancelled": true}))
                    .map_err(|e| e.to_string()),
                Err(_) => Err("daemon paths lock failed".to_owned()),
            }
        }
        Some("task_status") => {
            let id = request["id"].as_str().unwrap_or("");
            match state.manager.task(id) {
                Ok(task) => Ok(json!(task)),
                Err(e) => Err(e.to_string()),
            }
        }
        Some("update_progress") => {
            let id = request["id"].as_str().unwrap_or("");
            let stage = request["stage"].as_str().unwrap_or("");
            let progress = request["progress"].as_u64().unwrap_or(0) as u8;
            match state.paths.read() {
                Ok(paths) => state
                    .manager
                    .update(&paths, id, stage, progress)
                    .map(|task| json!(task))
                    .map_err(|e| e.to_string()),
                Err(_) => Err("daemon paths lock failed".to_owned()),
            }
        }
        Some("finish_task") => {
            let id = request["id"].as_str().unwrap_or("");
            let success = request["success"].as_bool().unwrap_or(true);
            let error_msg = request["error_msg"].as_str();
            let result: Result<(), String> = if success {
                Ok(())
            } else {
                Err(error_msg.unwrap_or("unknown error").to_owned())
            };
            match state.paths.read() {
                Ok(paths) => state
                    .manager
                    .finish(&paths, id, &result)
                    .map(|task| json!(task))
                    .map_err(|e| e.to_string()),
                Err(_) => Err("daemon paths lock failed".to_owned()),
            }
        }
        Some("delete_task") => delete_task_rpc(state, request),
        Some("shutdown") => {
            SHUTDOWN.store(true, Ordering::Relaxed);
            box_server_core::remove_discovery();
            Ok(json!({ "ok": true }))
        }
        _ => return json!({"ok": false, "error": "unknown method"}),
    };
    match result {
        Ok(value) => json!({"ok": true, "result": value}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn get_info() -> Result<Value, String> {
    let config = read_config()?;
    let mut info = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "buildStamp": env!("DSHBOX_BUILD_STAMP"),
        "pid": std::process::id(),
        "runtimeDirectory": config.runtime_directory,
        "githubMirror": config.github_mirror,
        "npmRegistry": config.npm_registry,
    });
    if let Some(root) = config.runtime_directory.as_ref() {
        let containers = box_containers::scan_containers(root).map(|items| items.len()).unwrap_or(0);
        info["containers"] = json!(containers);
        info["repositoryEntries"] = json!(scan_repository(std::path::Path::new(root)).len());
        info["bundles"] = json!(read_bundles(std::path::Path::new(root)).len());
        info["dshVersions"] = json!(installed_versions(root).map(|items| items.len()).unwrap_or(0));
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

fn list_containers(state: &DaemonState) -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut containers = box_containers::scan_containers(&root)?
        .into_values()
        .collect::<Vec<_>>();
    let running = state_running_containers(&state.containers);
    for container in &mut containers {
        container.status = if running.contains(&container.id) {
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

/// Container ids owned by this daemon process (status only; the daemon's
/// host registry grows as start/stop RPCs land).
fn state_running_containers(manager: &ContainerManager) -> Vec<String> {
    manager.running.lock().unwrap().keys().cloned().collect()
}

fn list_bundles() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(json!(read_bundles(std::path::Path::new(&root))))
}

fn list_repository_extensions() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(json!(scan_repository(std::path::Path::new(&root))))
}

/// Debugging aid: every repository entry paired with the ids of the
/// containers and built templates currently linked to it. Reads from
/// `references.json` exactly as it sits on disk (no reconciliation) —
/// the read path is fast on purpose, and `plugin rm` / `plugin prune`
/// / `template rm` / `container rm` all run `reconcile_owner_index`
/// before they mutate.
fn list_repository_reference_counts_rpc() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let entries = scan_repository(std::path::Path::new(&root));
    let references = box_extensions::read_references(std::path::Path::new(&root));
    let rows: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            let owners = references.get(&entry.id);
            let containers: Vec<String> = owners
                .map(|set| set.containers.iter().cloned().collect())
                .unwrap_or_default();
            let templates: Vec<String> = owners
                .map(|set| set.templates.iter().cloned().collect())
                .unwrap_or_default();
            json!({
                "id": entry.id,
                "name": entry.name,
                "kind": entry.kind,
                "version": entry.version,
                "containers": containers,
                "templates": templates,
            })
        })
        .collect();
    Ok(json!(rows))
}

fn list_installed_dsh_versions() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(json!(installed_versions(&root)?))
}

fn detect_toolchains() -> Result<Value, String> {
    let runtime = bundled_runtime()?;
    Ok(json!([
        { "id": "node", "name": "Node.js", "systemVersion": null, "managedVersion": runtime.node_version },
        { "id": "npm", "name": "npm", "systemVersion": null, "managedVersion": runtime.npm_version },
        { "id": "pnpm", "name": "pnpm", "systemVersion": null, "managedVersion": runtime.pnpm_version },
    ]))
}

/// Enqueue a build task on the daemon queue and return the task record
/// immediately; the client polls `task_status` for progress.
fn enqueue_build(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let parsed: BuildImageRequest = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid build request: {error}"))?;
    let params = json!({
        "scriptPath": parsed.script_path.clone(),
        "outputPath": parsed.output_path.clone(),
        "containerName": parsed.container_name.clone(),
    });
    enqueue_task_worker(
        state,
        "image-build",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| build_image_from_script(parsed, task),
    )
}

/// Enqueue `work` as a daemon-owned task and return the task record right
/// away; the client polls `task_status`. Every long-running RPC uses this
/// path so the daemon is the only process that executes business logic.
fn enqueue_task_worker(
    state: &DaemonState,
    kind: &str,
    resource_keys: Vec<String>,
    params: Value,
    work: impl FnOnce(&TaskContext) -> Result<(), String> + Send + 'static,
) -> Result<Value, String> {
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let task = state.manager.enqueue(&paths, kind, resource_keys, params)?;
    spawn_task_worker(state, &task, work);
    Ok(json!(task))
}

/// Spawn one daemon worker for a queued task, wiring the daemon notifier.
/// The worker only talks to daemon-owned state; clients stay thin.
fn spawn_task_worker(
    state: &DaemonState,
    task: &TaskRecord,
    work: impl FnOnce(&TaskContext) -> Result<(), String> + Send + 'static,
) {
    let manager = state.manager.clone();
    // Snapshot the paths under the lock; a poisoned lock leaves nothing
    // usable to run, so drop the worker instead of racing a broken queue.
    let paths = match state.paths.read() {
        Ok(paths) => paths.clone(),
        Err(_) => return,
    };
    let task_id = task.id.clone();
    std::thread::spawn(move || {
        let notifier = DaemonNotifier::from_paths(manager.clone(), paths.clone());
        run_queued(&manager, &paths, std::sync::Arc::new(notifier), &task_id, work);
    });
}

fn enqueue_dsh_catalog_refresh(state: &DaemonState) -> Result<Value, String> {
    enqueue_task_worker(
        state,
        "dsh-catalog-refresh",
        vec!["repository:dsh-versions".to_owned()],
        json!({}),
        |_task| refresh_dsh_catalog(),
    )
}

fn enqueue_pull_template(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let ref_value = request["ref"].as_str().unwrap_or("").to_owned();
    if ref_value.is_empty() {
        return Err("expected a template reference (e.g. `github.com/owner/repo[:tag|@ref]`); a missing `:tag` defaults to `latest`".to_owned());
    }
    // Resolve the version slug up front so the task is tagged with the
    // same `runtime:<version>` key the Harness tab watches for busy state.
    let version = parse_template_ref(&ref_value)
        .map_err(|error| format!("invalid template reference: {error}"))?
        .version;
    let params = json!({ "ref": ref_value.clone() });
    enqueue_task_worker(
        state,
        "template-pull",
        vec![format!("runtime:{version}"), "repository:dsh-versions".to_owned()],
        params,
        move |task| {
            // The clone is the long pole; poll the queued task's cancel flag
            // so users can abort a slow pull from the UI.
            let cancel_manager = task.manager.clone();
            let cancel_id = task.task_id.clone();
            pull_template_with_cancel(ref_value, move || {
                cancel_manager
                    .task(&cancel_id)
                    .map(|record| record.cancel_requested)
                    .unwrap_or(true)
            })
        },
    )
}

fn enqueue_repository_import(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let source = request["source"].as_str().unwrap_or("").to_owned();
    if source.is_empty() {
        return Err("expected a source path".to_owned());
    }
    let params = json!({ "source": source.clone() });
    enqueue_task_worker(
        state,
        "repository-extension-import",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| import_into_repository(task, Path::new(&source)).map(|_| ()),
    )
}

fn enqueue_repository_export(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let repository_id = request["repositoryId"].as_str().unwrap_or("").to_owned();
    let destination = request["destination"].as_str().unwrap_or("").to_owned();
    if repository_id.is_empty() || destination.is_empty() {
        return Err("expected repositoryId and destination".to_owned());
    }
    let params = json!({
        "repositoryId": repository_id.clone(),
        "destination": destination.clone(),
    });
    enqueue_task_worker(
        state,
        "repository-extension-export",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| export_repository_extension(&repository_id, &destination, task),
    )
}

fn enqueue_container_plugin_add(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let container_id = request["containerId"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("web").to_owned();
    let spec = request["spec"].as_str().unwrap_or("").to_owned();
    if container_id.is_empty() || spec.is_empty() {
        return Err("expected containerId and spec".to_owned());
    }
    let params = json!({
        "containerId": container_id.clone(),
        "profile": profile.clone(),
        "spec": spec.clone(),
    });
    enqueue_task_worker(
        state,
        "container-plugin-add",
        vec!["container:extensions".to_owned()],
        params,
        move |task| container_plugin_add(&container_id, &profile, &spec, task),
    )
}

fn enqueue_bundle_export(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let bundle_id = request["bundleId"].as_str().unwrap_or("").to_owned();
    let destination = request["destination"].as_str().unwrap_or("").to_owned();
    let mode = request["mode"].as_str().unwrap_or("quick").to_owned();
    if bundle_id.is_empty() || destination.is_empty() {
        return Err("expected bundleId and destination".to_owned());
    }
    if !matches!(mode.as_str(), "quick" | "full") {
        return Err("mode must be quick or full".to_owned());
    }
    let params = json!({
        "bundleId": bundle_id.clone(),
        "destination": destination.clone(),
        "mode": mode.clone(),
    });
    enqueue_task_worker(
        state,
        "bundle-export",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| export_extension_bundle(&bundle_id, &destination, &mode, task),
    )
}

fn enqueue_bundle_import(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let archive = request["archive"].as_str().unwrap_or("").to_owned();
    let conflict = request["conflict"].as_str().unwrap_or("keep").to_owned();
    if archive.is_empty() {
        return Err("expected an archive path".to_owned());
    }
    if !matches!(conflict.as_str(), "keep" | "overwrite") {
        return Err("conflict must be keep or overwrite".to_owned());
    }
    let params = json!({ "archive": archive.clone(), "conflict": conflict.clone() });
    enqueue_task_worker(
        state,
        "bundle-import",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| import_extension_bundle(&archive, &conflict, task),
    )
}

/// Materialize a template container and start its DSH host inside one
/// daemon task; the client resolves the new container's id and URL via
/// `list_containers` + `container_url` after the task settles.
fn enqueue_template_container(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let parsed: CreateTemplateContainerRequest = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid template container request: {error}"))?;
    let params = json!({
        "name": parsed.name.clone(),
        "template": parsed.template.clone(),
        "profile": parsed.profile.clone(),
    });
    let containers = state.containers.clone();
    enqueue_task_worker(
        state,
        "template-container",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| {
            let container = materialize_template_container(parsed, task)?;
            let url = start_dsh_container_inner(&container.id, &containers.running, Some(task))?;
            task.update(format!("Container {} is ready", container.id), 100);
            task.log(&format!("container url: {url}"));
            Ok(())
        },
    )
}

/// Returns the resource list of a built template (the metadata-only form
/// produced by `dshbox build`), for `dshbox template show`.
fn read_template_list(name: &str) -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let list = box_dsh_versions::read_built_template(&root, name)?
        .ok_or_else(|| format!("template `{name}` is not a built template"))?;
    Ok(json!(list))
}

/// Rich metadata for one built template: build timestamp, template version,
/// content-addressed id (manifest hash), base, profile, resource count, and
/// the parsed labels. Script templates (pulled/imported) return a slimmer
/// payload with just name, id, and imported timestamp — they have no build
/// metadata to speak of.
fn template_info(name: &str) -> Result<Value, String> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    if name.is_empty() {
        return Err("expected a template name".to_string());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;

    let index = read_template_index(&root);
    let entry = index
        .get(name)
        .ok_or_else(|| format!("template `{name}` not found"))?;

    let entry = entry.clone();

    // Script templates have no built-template manifest.
    if !entry.built {
        return Ok(json!({
            "name": entry.name,
            "id": entry.id,
            "profile": entry.profile,
            "harnessRef": entry.harness_ref,
            "built": false,
            "importedAt": entry.imported_at,
        }));
    }

    let built = read_built_template(&root, name)?
        .ok_or_else(|| format!("template `{name}` is marked built but has no manifest"))?;

    // Format ISO-ish timestamp from UNIX epoch seconds (no chrono dep).
    let created_iso = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(built.created_at))
        .map(|ts| {
            let secs = ts
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let days = secs / 86400;
            let tod = secs % 86400;
            let h = tod / 3600;
            let m = (tod % 3600) / 60;
            let s = tod % 60;

            let z: i64 = days as i64 + 719468;
            let era = if z >= 0 { z } else { z - 146096 } / 146096;
            let doe = z - era * 146096;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe as i32 + (era * 400) as i32;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let mut month = mp as i32 + 1;
            let mut year = y as i32;
            if mp >= 10 {
                month -= 12;
                year += 1;
            }
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                year, month, d, h, m, s
            )
        })
        .unwrap_or_else(|| built.created_at.to_string());

    Ok(json!({
        "name": built.name,
        "id": entry.id,
        "built": true,
        "base": built.base,
        "profile": built.profile,
        "harnessRef": built.harness_ref,
        "schemaVersion": built.schema_version,
        "createdAt": built.created_at,
        "createdAtIso": created_iso,
        "resources": built.resources.len(),
        "labels": built.labels,
    }))
}

/// Parse a string array parameter.
fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an optional string parameter (null/absent -> None, empty -> None).
fn optional_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

fn uninstall_dsh_version_rpc(request: &Value) -> Result<Value, String> {
    let version = request["version"].as_str().unwrap_or("").to_owned();
    uninstall_dsh_version(&version)?;
    Ok(json!({ "version": version }))
}

fn remove_repository_extension_rpc(request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    remove_repository_extension(&id)?;
    Ok(json!({ "id": id }))
}

fn delete_extension_bundle_rpc(request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    delete_extension_bundle(&id)?;
    Ok(json!({ "id": id }))
}

fn stop_container_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    stop_dsh_container(&id, &state.containers)?;
    Ok(json!({ "id": id, "stopped": true }))
}

fn container_url_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let running = state
        .containers
        .running
        .lock()
        .map_err(|_| "container manager lock failed".to_owned())?;
    match running.get(&id) {
        Some(host) => Ok(json!({ "id": id, "url": host.url })),
        None => Err(format!("container is not running: {id}")),
    }
}

fn save_mirror_settings_rpc(request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    config.github_mirror = optional_string(&request["githubMirror"]);
    config.npm_registry = optional_string(&request["npmRegistry"]);
    write_config(&config)?;
    Ok(json!({ "saved": true }))
}

fn save_runtime_directory_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    let directory = optional_string(&request["runtimeDirectory"]);
    if directory.is_none() {
        return Err("runtime directory cannot be empty".to_owned());
    }
    config.runtime_directory = directory;
    write_config(&config)?;
    // Re-derive the in-memory box paths so the task queue and every later
    // worker resolve the newly chosen storage root without a restart (a
    // daemon started before onboarding must adopt the new directory).
    state.adopt_config(&config)?;
    // Onboarding path: the daemon startup copy deferred because no runtime
    // directory was configured yet, so vendor the bundled plugins now.
    if let Err(error) = crate::state::initialize_bundled_plugins() {
        return Err(format!("cannot vendor bundled plugins: {error}"));
    }
    Ok(json!({ "saved": true, "restartRequired": true }))
}

fn enqueue_task(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let kind = request["kind"].as_str().unwrap_or("");
    let resource_keys: Vec<String> = request["resource_keys"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    state
        .manager
        .enqueue(&paths, kind, resource_keys, params)
        .map(|task| json!(task))
        .map_err(|e| e.to_string())
}

fn create_container_rpc(request: &Value) -> Result<Value, String> {
    let name = request["name"].as_str().unwrap_or("").to_owned();
    let version = request["version"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    create_dsh_container_sync(&name, &version, &profile).map(|container| json!(container))
}

fn enqueue_container_start(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("expected a container id".to_owned());
    }
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
}

fn enqueue_container_stop(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("expected a container id".to_owned());
    }
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
}

fn enqueue_container_rebuild(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("expected a container id".to_owned());
    }
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
}

/// Manual restart of a container that has been marked `Crashed` (or
/// `Stopped`) by the health watcher. Unlike `rebuild`, this keeps the
/// existing build artifacts and just re-spawns the host. The host
/// watcher detects the new PID automatically because
/// `start_dsh_container_inner` always allocates a fresh record before
/// spawning the readiness probe.
fn enqueue_container_restart(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("expected a container id".to_owned());
    }
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
}

fn delete_container_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    stop_dsh_container(&id, &state.containers)?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Reconcile before mutating so a stale file does not cause the
    // owner map to drift from the canonical sources we're about to
    // release.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
    let directory = Path::new(&root).join("instances").join(&id);
    if !directory.is_dir() {
        return Err(format!("container not found: {id}"));
    }
    // Release the container from each repository plugin it currently
    // references. Skills are skipped (they have no owner bookkeeping);
    // local / tarball installs have no `repository_id` (the remove
    // call is a no-op for them).
    let container = box_containers::DshContainer {
        id: id.clone(),
        name: String::new(),
        version: String::new(),
        profile: "web".to_owned(),
        template: None,
        directory: directory.to_string_lossy().into_owned(),
        status: String::new(),
    };
    for record in box_extensions::read_extension_records(&container) {
        if record.kind != box_extensions::ExtensionKind::Plugin {
            continue;
        }
        if let Some(repository_id) = record.repository_id {
            box_extensions::remove_reference_owner(
                Path::new(&root),
                &repository_id,
                box_extensions::ReferenceKind::Container,
                &id,
            )?;
        }
    }
    std::fs::remove_dir_all(&directory)
        .map_err(|error| format!("cannot remove container: {error}"))?;
    // Data payloads are temporary storage that follows container lifecycles
    // (no reference counting): garbage-collect the store now that this
    // container's usage records are gone.
    let _ = crate::data::prune_orphaned_data();
    Ok(json!({ "id": id, "deleted": true }))
}

/// Compose the full container description used by `dshbox container describe`.
/// Reuses the on-disk `DshContainer` summary as the base, then enriches it
/// with live runtime signals (URL, host PID) and the same extensions scan
/// the desktop details panel uses (`scan_container_extensions`). The result
/// is serialised through `box_api::ContainerDescription` so the wire shape
/// is identical for every consumer (CLI text, CLI `--json`, future UI).
fn describe_container_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
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
        .map(|host| host.url.clone());
    let host_pid = read_live_host_pid(&container);
    let status = if host_pid.is_some() { "running" } else { "stopped" };
    let extensions = scan_container_extensions(&container);
    let description = ContainerDescription {
        id: container.id.clone(),
        name: container.name.clone(),
        version: container.version.clone(),
        profile: container.profile.clone(),
        template: container.template.clone(),
        directory: container.directory.clone(),
        status: status.to_owned(),
        url,
        host_pid,
        extensions: serde_json::to_value(&extensions)
            .map_err(|error| format!("cannot serialize extensions: {error}"))?,
    };
    let value = serde_json::to_value(&description)
        .map_err(|error| format!("cannot serialize description: {error}"))?;
    Ok(value)
}

/// Read the PID file and confirm the host process is still alive (Unix
/// `kill -0`; Windows `tasklist`). Mirrors `is_host_alive` from
/// `box_containers` but returns the PID rather than a boolean — callers
/// that only need a flag still go through the box-containers helper.
fn read_live_host_pid(container: &box_containers::DshContainer) -> Option<u32> {
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
fn is_safe_workspace_relative_path(value: &str) -> bool {
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

fn enqueue_container_extension_add(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("web").to_owned();
    let source = request["source"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) || !box_foundation::is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    if source.trim().is_empty() {
        return Err("extension source is required".to_owned());
    }
    let params = json!({
        "id": id.clone(),
        "profile": profile.clone(),
        "source": source.clone(),
    });
    enqueue_task_worker(
        state,
        "container-extension-add",
        vec![format!("container:{id}")],
        params,
        move |task| install_container_extension(&id, &profile, &source, task),
    )
}

fn enqueue_workspace_extension_import(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let relative_path = request["relativePath"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id)
        || !is_safe_workspace_relative_path(&relative_path)
    {
        return Err("invalid workspace extension path".to_owned());
    }
    let params = json!({ "id": id.clone(), "relativePath": relative_path.clone() });
    enqueue_task_worker(
        state,
        "workspace-extension-import",
        vec![format!("container:{id}"), "repository:extensions".to_owned()],
        params,
        move |task| import_workspace_extension(&id, &relative_path, task),
    )
}

fn enqueue_container_extension_copy(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().map(str::to_owned);
    let repository_id = request["repositoryId"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id)
        || !box_foundation::is_safe_identifier(&repository_id)
        || profile
            .as_deref()
            .is_some_and(|value| !box_foundation::is_safe_identifier(value))
    {
        return Err("invalid extension copy request".to_owned());
    }
    let params = json!({ "id": id.clone(), "repositoryId": repository_id.clone() });
    enqueue_task_worker(
        state,
        "container-extension-copy",
        vec!["repository:extensions".to_owned(), format!("container:{id}")],
        params,
        // `container_extension_copy` is invoked from the resources page to
        // manually link a plugin into one container; no template is in
        // play, so we record the container as the owner of the
        // repository entry (None template_id triggers that branch in
        // `link_repository_extension`).
        move |task| link_repository_extension(&id, profile.as_deref(), &repository_id, None, task),
    )
}

fn enqueue_plugin_export(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let source_container_id = request["sourceContainerId"].as_str().unwrap_or("").to_owned();
    let source_path = request["sourcePath"].as_str().unwrap_or("").to_owned();
    let destination = request["destination"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&source_container_id)
        || source_path.trim().is_empty()
        || destination.trim().is_empty()
    {
        return Err("invalid plugin export request".to_owned());
    }
    let params = json!({ "sourceContainerId": source_container_id.clone(), "sourcePath": source_path.clone(), "destination": destination.clone() });
    enqueue_task_worker(
        state,
        "plugin-export",
        vec![format!("container:{source_container_id}")],
        params,
        move |task| {
            export_repository_plugin(&source_container_id, &source_path, &destination, task)
        },
    )
}

fn remove_repository_plugin_rpc(request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    let name = request["name"].as_str().unwrap_or("").to_owned();
    remove_repository_plugin(&id, &profile, &name)?;
    Ok(json!({ "id": id, "removed": true }))
}

fn enqueue_container_bundle_install(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    let bundle_id = request["bundleId"].as_str().unwrap_or("").to_owned();
    let conflict = request["conflict"].as_str().unwrap_or("keep").to_owned();
    if !box_foundation::is_safe_identifier(&id)
        || !box_foundation::is_safe_identifier(&profile)
        || !box_foundation::is_safe_identifier(&bundle_id)
        || !matches!(conflict.as_str(), "overwrite" | "keep")
    {
        return Err("invalid bundle install request".to_owned());
    }
    let params = json!({
        "id": id.clone(),
        "profile": profile.clone(),
        "bundleId": bundle_id.clone(),
        "conflict": conflict.clone(),
    });
    enqueue_task_worker(
        state,
        "container-bundle-install",
        vec![format!("container:{id}")],
        params,
        move |task| install_container_bundle(&id, &profile, &bundle_id, &conflict, task),
    )
}

fn delete_task_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("");
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    state
        .manager
        .remove(&paths, id)
        .map(|_| json!({ "id": id, "deleted": true }))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    //! Tests for the dispatch table; see `describe_container_rpc` below
    //! for the most thorough coverage. The `env_lock` helper at the
    //! crate level serialises every test that mutates `HOME` (or the
    //! daemon's config dir) so they cannot trample each other's
    //! persisted config files when cargo runs the suite in parallel.

    use super::*;
    use crate::test_support::env_lock;
    use box_containers::{container_directory, host_pid_path};
    use box_foundation::{read_config, BoxConfig, BoxPaths};
    use std::{
        env, fs,
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    /// Builds a runtime directory with one container + a PID file pointing
    /// at a child the test owns. Returns the daemon state and the
    /// container id. The caller MUST hold `env_lock()` for the entire
    /// lifetime of the test (including `cleanup`) — see the module docs.
    fn setup(include_live_pid: bool) -> (DaemonState, String, PathBuf, PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let home = env::temp_dir().join(format!("dshboxd-dispatch-home-{stamp}"));
        let runtime = env::temp_dir().join(format!("dshboxd-dispatch-runtime-{stamp}"));
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        // Persist the runtime directory to a fake $HOME/.dsh-box/config.json.
        let config_dir = home.join(".dsh-box");
        fs::create_dir_all(&config_dir).unwrap();
        let config = BoxConfig {
            runtime_directory: Some(runtime.to_string_lossy().into_owned()),
            ..Default::default()
        };
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        // Same env var that box_foundation::read_config looks up.
        // SAFETY: tests are single-threaded within a process.
        unsafe { env::set_var("DSHBOX_CONFIG_DIR", &config_dir) };
        unsafe { env::set_var("HOME", &home) };

        let id = format!("container-{stamp}");
        let directory = container_directory(runtime.to_string_lossy().as_ref(), &id);
        fs::create_dir_all(directory.join("state")).unwrap();
        fs::create_dir_all(directory.join("profile/profiles")).unwrap();
        fs::write(
            directory.join("container.json"),
            serde_json::json!({
                "id": id,
                "name": "dsh-test",
                "version": "latest",
                "profile": "web",
                "template": "dsh-test",
            })
            .to_string(),
        )
        .unwrap();

        let pid = if include_live_pid {
            // Spawn a child whose PID we own; `kill -0` will succeed for
            // the lifetime of the test. Reaped at the end.
            let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let pid = child.id();
            // Stash the child for the test to reap later; storing into the
            // env is too awkward, so we leak the handle and rely on the
            // process being short-lived enough that the OS cleans up.
            Box::leak(Box::new(child));
            fs::write(host_pid_path(&box_containers::DshContainer {
                id: id.clone(),
                name: "dsh-test".into(),
                version: "latest".into(),
                profile: "web".into(),
                template: Some("dsh-test".into()),
                directory: directory.to_string_lossy().into_owned(),
                status: "running".into(),
            }), pid.to_string()).unwrap();
            Some(pid)
        } else {
            None
        };
        let _ = pid; // silence unused warning when include_live_pid is false

        // Build the DaemonState directly (avoids relying on the global
        // discovery file) using the same fields DaemonState::load wires up.
        let config = read_config().unwrap();
        let paths = BoxPaths::from_config(&config).unwrap();
        let state = DaemonState {
            manager: box_scheduler::TaskManager::default(),
            paths: RwLock::new(paths),
            containers: Arc::new(ContainerManager::default()),
            resources: box_state::ResourceStateManager::default(),
        };
        (state, id, home, runtime)
    }

    fn cleanup(home: &PathBuf, runtime: &PathBuf) {
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(runtime);
    }

    #[test]
    fn describe_container_marks_running_when_pid_alive() {
        let _guard = env_lock();
        let (state, id, home, runtime) = setup(true);
        let response = describe_container_rpc(&state, &json!({ "id": id })).unwrap();
        assert_eq!(response["id"], id);
        assert_eq!(response["name"], "dsh-test");
        assert_eq!(response["version"], "latest");
        assert_eq!(response["profile"], "web");
        assert_eq!(response["template"], "dsh-test");
        assert_eq!(response["status"], "running");
        // URL is only set when the daemon actually started the host (no
        // running entry in the registry); `host_pid` is the live probe.
        assert!(response["url"].is_null());
        assert!(response["hostPid"].is_u64());
        // Extensions object is present (empty profile scan).
        assert!(response["extensions"].is_object());
        assert!(response["extensions"]["profiles"].is_array());
        cleanup(&home, &runtime);
    }

    #[test]
    fn describe_container_marks_stopped_when_pid_file_missing() {
        let _guard = env_lock();
        let (state, id, home, runtime) = setup(false);
        let response = describe_container_rpc(&state, &json!({ "id": id })).unwrap();
        assert_eq!(response["status"], "stopped");
        assert!(response["url"].is_null());
        assert!(response["hostPid"].is_null());
        cleanup(&home, &runtime);
    }

    #[test]
    fn describe_container_rejects_unsafe_identifier() {
        let _guard = env_lock();
        let (state, _id, home, runtime) = setup(false);
        let err = describe_container_rpc(&state, &json!({ "id": "../etc/passwd" })).unwrap_err();
        assert!(err.contains("invalid container id"), "got: {err}");
        cleanup(&home, &runtime);
    }

    #[test]
    fn describe_container_returns_error_for_unknown_id() {
        let _guard = env_lock();
        let (state, _id, home, runtime) = setup(false);
        let err = describe_container_rpc(&state, &json!({ "id": "container-missing" })).unwrap_err();
        assert!(err.contains("container not found"), "got: {err}");
        cleanup(&home, &runtime);
    }
}