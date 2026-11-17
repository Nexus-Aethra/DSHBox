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
    container_list_plugins, container_plugin_add, export_repository_extension,
    export_repository_plugin, import_source, import_workspace_extension,
    install_container_extension, link_repository_extension, prune_unused_repository_extensions,
    remove_repository_extension, remove_repository_plugin,
};
use crate::host::{self, HostState};
use crate::lifecycle::{
    rebuild_dsh_container_with_task, start_dsh_container_inner, stop_dsh_container,
};
use crate::sealed::{
    build_sealed_template_from_script, create_container_from_sealed, export_sealed_template,
    import_sealed_template, list_sealed_templates, prune_sealed_template_snapshots,
    read_sealed_template, remove_sealed_template, sealed_template_info, template_root_by_name,
};
use crate::state::{bundled_runtime, ContainerManager, DaemonNotifier, DaemonState};
use crate::versions::{
    fetch_remote_dsh_tags, list_dsh_versions_derived, list_installed_dsh_versions,
    pull_template_with_cancel, refresh_dsh_catalog, uninstall_dsh_version,
};
use box_api::{BuildImageRequest, ContainerDescription, CreateTemplateContainerRequest};
use box_dsh_versions::{
    installed_versions, parse_template_ref, read_built_template, read_template_index,
};
use box_extensions::{read_bundles, scan_container_extensions, scan_repository};
use box_foundation::{now_seconds, read_config, write_config};
use box_scheduler::{run_queued, TaskContext, TaskRecord};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

mod rpc_containers;
mod rpc_extensions;
mod rpc_settings;
mod rpc_templates;

// This file is the routing table and the task-enqueue primitives; the four
// rpc_* modules are the handlers it routes to. pub use keeps every existing
// crate::dispatch::x path working.
pub use rpc_containers::*;
pub use rpc_extensions::*;
pub use rpc_settings::*;
pub use rpc_templates::*;


/// Set when a client asks the daemon to stop (build-batch mismatch during
/// an upgrade); `main.rs` exits after the response is written back.
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// What an RPC handler returns. The dispatch table wraps each match arm in
/// one of these so the single entry point can format the HTTP response
/// without per-handler branching.
#[derive(Debug)]
pub enum HandlerResult {
    /// Synchronous reply: data is serialised directly into the response.
    Sync(Value),
    /// Asynchronous reply: caller polls task status and listens on /events.
    Async(TaskRecord),
}

/// Function shape for every entry in the dispatch table. Sync handlers
/// return JSON inline; async handlers enqueue a worker and return the
/// `TaskRecord` immediately.
pub type Handler = fn(&DaemonState, &Value) -> Result<HandlerResult, String>;

/// Catalogue of every method this daemon answers. The current dispatch
/// driver uses an inline `match`, but the table is exposed so introspection
/// endpoints (`task_state_machine` RPC, CLI `dshbox rpc list`) can list
/// available methods without re-parsing the match body.
pub fn dispatch_table() -> BTreeMap<&'static str, Handler> {
    use HandlerResult::Sync;
    let mut table: BTreeMap<&'static str, Handler> = BTreeMap::new();
    table.insert("ping", |_state, _req| {
        Ok(Sync(json!({
            "pid": std::process::id(),
            "status": "running",
            "startedAt": now_seconds(),
            "runtime": bundled_runtime().map(|_| "ready").unwrap_or("missing"),
        })))
    });
    table.insert("task_state_machine", |_state, _req| {
        Ok(Sync(box_scheduler::task_state_machine_definition()))
    });
    table
}

/// Dispatch one request against the daemon state. Every handler returns a
/// `{"ok": bool, ...}` frame; errors never panic the connection thread.
///
/// Sync handlers wrap their JSON payload in `HandlerResult::Sync`; async
/// handlers enqueue a worker and return `HandlerResult::Async(TaskRecord)`
/// so the caller can subscribe to `/events` for progress.
pub(crate) fn dispatch(state: &DaemonState, request: &Value) -> Value {
    use HandlerResult::Sync;
    let method = request["method"].as_str().unwrap_or("?");
    debug!(method, "rpc request received");
    let result: Result<HandlerResult, String> = match request["method"].as_str() {
        Some("node_enroll") => crate::nodes_rpc::node_enroll(state, request).map(Sync),
        Some("node_register") => crate::nodes_rpc::node_register(state, request).map(Sync),
        Some("node_heartbeat") => crate::nodes_rpc::node_heartbeat(state, request).map(Sync),
        Some("node_list") => crate::nodes_rpc::node_list(state, request).map(Sync),
        Some("node_get") => crate::nodes_rpc::node_get(state, request).map(Sync),
        Some("node_echo") => crate::nodes_rpc::node_echo(state, request).map(Sync),
        Some("node_report") => crate::nodes_rpc::node_report(state, request).map(Sync),
        Some("node_deregister") => crate::nodes_rpc::node_deregister(state, request).map(Sync),
        // Stage B1: the control plane's index of prepared bases per node. The
        // list is a control-plane read; the report is the only writer and is
        // reachable only with a node session (see NODE_METHODS).
        Some("node_artifact_list") => {
            crate::nodes_rpc::node_artifact_list(state, request).map(Sync)
        }
        Some("node_artifact_report") => {
            crate::nodes_rpc::node_artifact_report(state, request).map(Sync)
        }
        Some("ping") => Ok(Sync(json!({
            "pid": std::process::id(),
            "status": "running",
            "startedAt": now_seconds(),
            "runtime": bundled_runtime().map(|_| "ready").unwrap_or("missing"),
        }))),
        Some("get_info") => get_info().map(Sync),
        // Update discovery. The read is cheap and never touches the network;
        // the explicit check and the download are the two that do.
        Some("get_update_state") => crate::updates::get_update_state_rpc(state).map(Sync),
        Some("check_update") => crate::updates::check_update_rpc(state).map(Sync),
        Some("list_update_releases") => crate::updates::list_update_releases_rpc().map(Sync),
        Some("enqueue_update_download") => crate::updates::enqueue_update_download(state, request),
        Some("discard_update_download") => {
            crate::updates::discard_update_download(request).map(Sync)
        }
        Some("skip_update_version") => crate::updates::skip_update_version(request).map(Sync),
        Some("clear_update_skip") => crate::updates::clear_update_skip().map(Sync),
        Some("set_update_policy") => crate::updates::set_update_policy(request).map(Sync),
        Some("list_containers") => list_containers(state).map(Sync),
        Some("list_templates") => list_sealed_templates().map(|items| Sync(json!(items))),
        Some("read_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            read_sealed_template(&name).map(|text| Sync(json!({ "name": name, "text": text })))
        }
        Some("import_template") => {
            let archive = request["archive"].as_str().unwrap_or("").to_owned();
            let name = request["name"]
                .as_str()
                .map(str::to_owned)
                .filter(|value| !value.is_empty());
            import_sealed_template(&archive, name).map(|name| Sync(json!({ "name": name })))
        }
        Some("export_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            let destination = request["destination"]
                .as_str()
                .map(str::to_owned)
                .filter(|value| !value.is_empty());
            export_sealed_template(&name, destination).map(|path| Sync(json!({ "path": path })))
        }
        Some("remove_template") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            remove_sealed_template(&name).map(|_| Sync(json!({ "name": name, "removed": true })))
        }
        Some("list_bundles") => list_bundles().map(Sync),
        Some("list_repository_extensions") => list_repository_extensions().map(Sync),
        Some("list_repository_reference_counts") => {
            list_repository_reference_counts_rpc().map(Sync)
        }
        Some("list_installed_dsh_versions") => {
            list_installed_dsh_versions().map(|names| Sync(json!(names)))
        }
        Some("detect_toolchains") => detect_toolchains().map(Sync),
        Some("enqueue_build") => enqueue_build(state, request),
        Some("enqueue_task") => enqueue_task(state, request),
        Some("list_dsh_catalog") => {
            // The Harness tab is derived entirely from the template index;
            // `latest` and every installed harness show up here, and any
            // remote tags fetched by `refresh_dsh_catalog` (if the user
            // pressed "Load versions" first) are merged in.
            let remote = fetch_remote_dsh_tags().ok();
            list_dsh_versions_derived(remote).map(|items| Sync(json!(items)))
        }
        Some("uninstall_dsh_version") => uninstall_dsh_version_rpc(request).map(Sync),
        Some("remove_repository_extension") => remove_repository_extension_rpc(request).map(Sync),
        Some("prune_repository_extensions") => {
            prune_unused_repository_extensions().map(|removed| Sync(json!(removed)))
        }
        Some("prune_orphaned_data") => prune_orphaned_data().map(|removed| Sync(json!(removed))),
        Some("list_data_entries") => list_data_entries().map(|entries| Sync(json!(entries))),
        Some("container_list_plugins") => {
            let id = request["containerId"].as_str().unwrap_or("").to_owned();
            let profile = request["profile"].as_str().unwrap_or("web").to_owned();
            // `container_list_plugins` reads the profile manifest by the id it is
            // handed and has no state of its own, so the boundary is the only
            // place a name can be turned into that id. `dispatch` answers a
            // Value rather than a Result, so the failure is mapped here.
            match crate::resolve::resolve_container_key(state, &id) {
                Ok(id) => container_list_plugins(&id, &profile).map(|plugins| Sync(json!(plugins))),
                Err(error) => Err(error),
            }
        }
        Some("create_extension_bundle") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            let ids = string_array(&request["repositoryIds"]);
            create_extension_bundle(&name, &ids).map(|bundle| Sync(json!(bundle)))
        }
        Some("delete_extension_bundle") => delete_extension_bundle_rpc(request).map(Sync),
        Some("stop_container") => stop_container_rpc(state, request).map(Sync),
        Some("container_url") => container_url_rpc(state, request).map(Sync),
        // Page-debugging surface. browser_status and set_browser_path take no
        // container; the rest address one running container's headless session.
        Some("debug_browser_status") => crate::debug::browser_status_rpc().map(Sync),
        Some("debug_set_browser_path") => crate::debug::set_browser_path_rpc(request).map(Sync),
        Some("debug_open") => crate::debug::open_rpc(state, request).map(Sync),
        Some("debug_close") => crate::debug::close_rpc(state, request).map(Sync),
        Some("debug_capture_screenshot") => crate::debug::screenshot_rpc(state, request).map(Sync),
        Some("debug_query_elements") => crate::debug::query_elements_rpc(state, request).map(Sync),
        Some("debug_click_element") => crate::debug::click_element_rpc(state, request).map(Sync),
        Some("debug_click_at") => crate::debug::click_at_rpc(state, request).map(Sync),
        Some("debug_click_by_name") => crate::debug::click_by_name_rpc(state, request).map(Sync),
        Some("debug_type_text") => crate::debug::type_text_rpc(state, request).map(Sync),
        Some("debug_press_key") => crate::debug::press_key_rpc(state, request).map(Sync),
        Some("debug_page_text") => crate::debug::page_text_rpc(state, request).map(Sync),
        Some("debug_scroll") => crate::debug::scroll_rpc(state, request).map(Sync),
        Some("debug_set_viewport") => crate::debug::set_viewport_rpc(state, request).map(Sync),
        Some("debug_console") => crate::debug::console_rpc(state, request).map(Sync),
        Some("debug_navigate") => crate::debug::navigate_rpc(state, request).map(Sync),
        Some("debug_hover") => crate::debug::hover_rpc(state, request).map(Sync),
        Some("debug_evaluate") => crate::debug::evaluate_rpc(state, request).map(Sync),
        Some("apply_document") => crate::apply::apply_document_rpc(state, request).map(Sync),
        Some("save_mirror_settings") => save_mirror_settings_rpc(request).map(Sync),
        Some("save_github_token") => save_github_token_rpc(request).map(Sync),
        Some("save_runtime_directory") => save_runtime_directory_rpc(state, request).map(Sync),
        Some("refresh_dsh_catalog") => enqueue_dsh_catalog_refresh(state),
        Some("pull_template") => enqueue_pull_template(state, request),
        Some("harness_mirror_info") => harness_mirror_info_rpc(),
        // Purge goes through the scheduler like every other long job: the mirror
        // must not be deleted while a pull is writing to it.
        Some("purge_harness_mirror") => enqueue_purge_harness_mirror(state),
        Some("import_repository_extension") => enqueue_repository_import(state, request),
        Some("export_repository_extension") => enqueue_repository_export(state, request),
        Some("container_plugin_add") => enqueue_container_plugin_add(state, request),
        Some("export_bundle") => enqueue_bundle_export(state, request),
        Some("import_bundle") => enqueue_bundle_import(state, request),
        Some("create_container_from_template") => enqueue_template_container(state, request),
        Some("read_template_list") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            read_template_list(&name).map(Sync)
        }
        Some("template_info") => {
            let name = request["name"].as_str().unwrap_or("").to_owned();
            sealed_template_info(&name).map(Sync)
        }
        Some("prune_template_snapshots") => {
            prune_sealed_template_snapshots().map(|removed| Sync(json!({ "removed": removed })))
        }
        Some("plugin_dependency_graph") => plugin_dependency_graph_rpc(request).map(Sync),
        Some("list_container_workspaces") => {
            crate::workspaces::list_container_workspaces_rpc(state, request).map(Sync)
        }
        Some("add_container_workspace") => crate::workspaces::add_container_workspace_rpc(state, request),
        Some("list_container_resources") => {
            crate::resources::list_container_resources(state, request).map(Sync)
        }
        Some("list_resources") => crate::resources::list_resources(state, request).map(Sync),
        Some("list_resource_views") => {
            crate::resources::list_resource_views(state, request).map(Sync)
        }
        Some("list_installed_plugins") => {
            crate::plugins::list_installed_plugins(state, request).map(Sync)
        }
        Some("add_resource_view") => crate::resources::add_resource_view(state, request).map(Sync),
        Some("delete_resource_view") => {
            crate::resources::delete_resource_view(state, request).map(Sync)
        }
        Some("set_resource_view_enabled") => {
            crate::resources::set_resource_view_enabled(state, request).map(Sync)
        }
        Some("list_resource_type") => {
            crate::resources::list_resource_type(state, request).map(Sync)
        }
        Some("browse_container_paths") => {
            crate::resources::browse_container_paths(state, request).map(Sync)
        }
        Some("delete_resource") => crate::resources::delete_resource(state, request).map(Sync),
        Some("enqueue_resource_extract") => {
            crate::resources::enqueue_resource_extract(state, request)
        }
        Some("read_resource_tree") => {
            crate::resources::read_resource_tree(state, request).map(Sync)
        }
        Some("enqueue_resource_write") => crate::resources::enqueue_resource_write(state, request),
        Some("enqueue_resource_inject") => {
            crate::resources::enqueue_resource_inject(state, request)
        }
        Some("create_container") => create_container_rpc(request).map(Sync),
        Some("enqueue_container_start") => enqueue_container_start(state, request),
        Some("enqueue_container_stop") => enqueue_container_stop(state, request),
        Some("enqueue_container_rebuild") => enqueue_container_rebuild(state, request),
        Some("enqueue_container_restart") => enqueue_container_restart(state, request),
        Some("delete_container") => delete_container_rpc(state, request),
        Some("describe_container") => describe_container_rpc(state, request),
        Some("enqueue_container_extension_add") => enqueue_container_extension_add(state, request),
        Some("enqueue_workspace_extension_import") => {
            enqueue_workspace_extension_import(state, request)
        }
        Some("enqueue_container_extension_copy") => {
            enqueue_container_extension_copy(state, request)
        }
        Some("enqueue_plugin_export") => enqueue_plugin_export(state, request),
        Some("remove_repository_plugin") => remove_repository_plugin_rpc(state, request).map(Sync),
        Some("enqueue_container_bundle_install") => {
            enqueue_container_bundle_install(state, request)
        }
        Some("list_tasks") => match state.manager.list() {
            Ok(tasks) => Ok(Sync(json!(tasks))),
            Err(e) => Err(e.to_string()),
        },
        Some("cancel_task") => {
            let id = request["id"].as_str().unwrap_or("");
            match state.paths.read() {
                Ok(paths) => state
                    .manager
                    .request_cancel(&paths, id)
                    .map(|_| Sync(json!({"id": id, "cancelled": true})))
                    .map_err(|e| e.to_string()),
                Err(_) => Err("daemon paths lock failed".to_owned()),
            }
        }
        Some("task_status") => {
            let id = request["id"].as_str().unwrap_or("");
            match state.manager.task(id) {
                Ok(task) => Ok(Sync(json!(task))),
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
                    .map(|task| Sync(json!(task)))
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
                    .map(|task| Sync(json!(task)))
                    .map_err(|e| e.to_string()),
                Err(_) => Err("daemon paths lock failed".to_owned()),
            }
        }
        Some("delete_task") => delete_task_rpc(state, request).map(Sync),
        Some("shutdown") => {
            SHUTDOWN.store(true, Ordering::Relaxed);
            box_server_core::remove_discovery();
            Ok(Sync(json!({ "ok": true })))
        }
        Some("task_state_machine") => {
            // Look the entry up in the dispatch table so the BTreeMap stays
            // the canonical registration site; if a future refactor moves
            // this handler off the match, the table keeps it reachable.
            match dispatch_table().get("task_state_machine") {
                Some(handler) => handler(state, request),
                None => Err("task_state_machine handler missing".to_owned()),
            }
        }
        _ => return json!({"ok": false, "error": "unknown method"}),
    };
    match result {
        Ok(HandlerResult::Sync(value)) => {
            debug!(method, "rpc sync reply sent");
            json!({"ok": true, "result": value})
        }
        Ok(HandlerResult::Async(task)) => {
            info!(method, task_id = %task.id, kind = %task.kind, "rpc async task enqueued");
            json!({"ok": true, "task": task, "eventsUrl": "/events"})
        }
        Err(error) => {
            warn!(method, error = %error, "rpc request failed");
            json!({"ok": false, "error": error})
        }
    }
}

/// Container ids owned by this daemon process (status only; the daemon's
/// host registry grows as start/stop RPCs land).
fn state_running_containers(manager: &ContainerManager) -> Vec<String> {
    manager.running.lock().unwrap().keys().cloned().collect()
}

/// Enqueue `work` as a daemon-owned task and return the task record right
/// away; the client polls `task_status`. Every long-running RPC uses this
/// path so the daemon is the only process that executes business logic.
pub(crate) fn enqueue_task_worker(
    state: &DaemonState,
    kind: &str,
    resource_keys: Vec<String>,
    params: Value,
    work: impl FnOnce(&TaskContext) -> Result<(), String> + Send + 'static,
) -> Result<TaskRecord, String> {
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let task = state.manager.enqueue(&paths, kind, resource_keys, params)?;
    spawn_task_worker(state, &task, work);
    Ok(task)
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
    let events = state.events.clone();
    std::thread::spawn(move || {
        let notifier = DaemonNotifier::from_paths(manager.clone(), events);
        run_queued(
            &manager,
            &paths,
            std::sync::Arc::new(notifier),
            &task_id,
            work,
            None,
        );
    });
}

fn enqueue_bundle_export(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
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
    .map(HandlerResult::Async)
}

fn enqueue_bundle_import(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
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
    .map(HandlerResult::Async)
}

/// Parse an optional string parameter (null/absent -> None, empty -> None).
fn optional_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
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
#[path = "dispatch_wsl_vm_delete_tests.rs"]
mod wsl_vm_delete_tests;

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "dispatch_usable_url_tests.rs"]
mod usable_url_tests;