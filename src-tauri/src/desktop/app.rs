use box_dsh_context::{
    render_patch_yml, render_snapshot, DshContextFiles, DEFAULT_ORDER, PATCH_FILENAME,
    SNAPSHOT_FILENAME,
};
use box_containers::{
    container_directory, scan_containers, CreateDshContainerRequest, DshContainer,
};
use box_dsh_versions::{DshVersion};
use box_extensions::{scan_workspace_extensions, ExtensionBundle};
use box_foundation::{
    is_safe_identifier, normalize_optional_url, normalize_runtime_directory,
    now_seconds, read_config, strip_verbatim_prefix, write_config,
    BoxConfig, BoxPaths,
};
use box_scheduler::{run_queued, TaskContext, TaskManager, TaskNotifier, TaskRecord};
use box_server_core::{
    install_tray_autostart, install_user_service, restart_user_service, service_status,
    start_user_service, stop_user_service, ServiceStatus,
};
use box_state::{ResourceSnapshot, ResourceState, ResourceStateManager};
use box_toolchains::{is_known_toolchain, ToolchainStatus};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    thread,
    time::Duration,
};
use tauri::{Manager, WindowEvent};
use xz2::read::XzDecoder;

mod bundles;
mod commands;
mod containers;
mod rpc;
mod events;
mod extensions;
pub(crate) mod image;
pub(crate) mod lifecycle;
mod services;
mod tasks;
mod toolchains;
mod versions;

pub(crate) use bundles::*;
pub(crate) use containers::*;
pub(crate) use extensions::*;
pub(crate) use rpc::*;
pub(crate) use events::*;
pub(crate) use lifecycle::*;
pub(crate) use services::*;
pub(crate) use tasks::*;
pub(crate) use toolchains::*;
pub(crate) use versions::*;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedToolchain {
    id: String,
    source: String,
    path: String,
    #[serde(default)]
    arguments: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct BundledRuntimeManifest {
    node_version: String,
    pnpm_version: String,
    node_entry: String,
    npm_entry: String,
    pnpm_entry: String,
}

pub(crate) struct BundledRuntime {
    node_version: String,
    npm_version: String,
    pnpm_version: String,
    node: PathBuf,
    npm: PathBuf,
    pnpm: PathBuf,
}

static BUNDLED_RUNTIME: OnceLock<BundledRuntime> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolchainCommandRequest {
    id: String,
    args: Vec<String>,
    cwd: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AddContainerExtensionRequest {
    id: String,
    profile: String,
    source: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportRepositoryExtensionRequest { pub(crate) source: String }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopyRepositoryExtensionRequest { id: String, profile: Option<String>, repository_id: String }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportRepositoryExtensionRequest { pub(crate) repository_id: String, pub(crate) destination: String }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportWorkspaceExtensionRequest { id: String, relative_path: String }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportContainerPluginRequest {
    source_container_id: String,
    source_path: String,
    destination: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolchainCommandResult {
    path: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolchainInstallStatus {
    id: String,
    stage: String,
    log_path: String,
    lines: Vec<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub(crate) struct NodeRelease {
    version: String,
    files: Vec<String>,
}

/// Local container-manager stub kept only as the `tauri::State` slot type in
/// command signatures. The daemon owns container hosts now, so the running
/// map is gone; this type exists purely to keep the frontend-facing command
/// surface unchanged.
#[derive(Default)]
pub(crate) struct ContainerManager {}


pub(super) fn run() {
    if let Err(error) = run_inner() {
        write_startup_log(&format!("desktop startup failed: {error}"));
        panic!("DSH Box startup failed: {error}");
    }
}

/// Startup repair pass for the template system:
/// 1. Mirror every `runtimes/<tag>/source/` runtime into the template
///    index. Older installs used a separate writer that left the runtime
///    clone without an index entry; the Harness tab and the Container
///    dropdown both rely on the index, so this is required for them to
///    see the user's existing harnesses.
/// 2. Containers created before the template system are bound to the base
///    template of their harness version when one exists locally; containers
///    that cannot be bound fall back to the startup validation error.
///
/// Every step is best-effort and logged; failures never block startup.
fn repair_resources_on_startup(root: &str) {
    if let Ok(installed) = box_dsh_versions::installed_versions(root) {
        let index = box_dsh_versions::read_template_index(root);
        let already_indexed: std::collections::BTreeSet<String> = index
            .values()
            .filter_map(|entry| entry.harness_ref.clone())
            .collect();
        for tag in installed {
            if already_indexed.contains(&tag) {
                continue;
            }
            let ref_value = format!("github.com/deepseek-ai/deepseek-harness:{tag}");
            let body =
                format!("FROM {ref_value}\nPROFILE web\nNAME {ref_value}\nVERSION latest\n");
            match box_dsh_versions::write_template_with_entry(
                root,
                &ref_value,
                &body,
                Some(tag.clone()),
                "web",
                Some(ref_value.clone()),
                now_seconds(),
                box_dsh_versions::TemplateKind::Root,
            ) {
                Ok(entry) => write_startup_log(&format!(
                    "registered harness `{tag}` in template index ({})",
                    entry.id
                )),
                Err(error) => write_startup_log(&format!(
                    "cannot register harness `{tag}` in template index: {error}"
                )),
            }
        }
    }
    let instances = PathBuf::from(root).join("instances");
    let mut bound = 0usize;
    if let Ok(entries) = fs::read_dir(&instances) {
        for entry in entries.filter_map(Result::ok) {
            let container_file = entry.path().join("container.json");
            let Ok(metadata) = fs::read_to_string(&container_file) else {
                continue;
            };
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&metadata) else {
                continue;
            };
            if value["template"].is_string() || value["image"].is_string() {
                continue;
            }
            let Some(version) = value["version"].as_str().map(str::to_owned) else {
                continue;
            };
            let template_path =
                box_dsh_versions::templates_directory(root).join(format!("{version}.dsh"));
            if !template_path.is_file() {
                continue;
            }
            value["template"] = serde_json::Value::String(version.clone());
            let updated = serde_json::to_string_pretty(&value).unwrap_or(metadata);
            if fs::write(&container_file, updated).is_ok() {
                bound += 1;
                write_startup_log(&format!(
                    "bound container {} to template {version}",
                    entry.file_name().to_string_lossy()
                ));
            }
        }
    }
    if bound > 0 {
        write_startup_log(&format!("bound {bound} container(s) to base templates"));
    }
}

fn run_inner() -> Result<(), String> {
    let initial_container = env::args()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|arguments| arguments[0] == "--open-container")
        .map(|arguments| arguments[1].clone())
        .filter(|id| is_safe_identifier(id));
    tauri::Builder::default()
        .manage(ContainerManager::default())
        .manage(TaskManager::default())
        .manage(ResourceStateManager::default())
        .setup(move |app| {
            let resources = app
                .path()
                .resource_dir()
                .map_err(|error| error.to_string())?;
            write_startup_log(&format!("resource directory: {}", resources.display()));
            initialize_bundled_runtime(resources.clone()).map_err(|error| { write_startup_log(&format!("bundled runtime initialization failed: {error}")); error })?;
            // Vendored Cordis plugins live in the Tauri resource and need to be
            // copied into the runtime directory on first launch (and after every
            // resource change). Errors are non-fatal: a container can still start
            // without the plugin, just without structured container metadata in
            // the system prompt.
            if let Err(error) = initialize_bundled_plugins(&resources) {
                write_startup_log(&format!("bundled plugins initialization skipped: {error}"));
            }
            if let Ok(runtime) = bundled_runtime() {
                write_startup_log(&format!(
                    "bundled runtime ready: node {} at {}, npm {}, pnpm {}",
                    runtime.node_version,
                    runtime.node.display(),
                    runtime.npm.display(),
                    runtime.pnpm.display()
                ));
            }
            let server = bundled_server_path(
                &app.path()
                    .resource_dir()
                    .map_err(|error| error.to_string())?,
            );
            if server.is_file() {
                #[cfg(unix)]
                link_daemon_into_path(&server);
                if let Err(error) = install_user_service(&server) {
                    write_startup_log(&format!("dshboxd service installation failed: {error}"));
                    // Platforms without a per-user service manager (macOS),
                    // broken systemd units, and Windows where the
                    // scheduled task creation was rejected all still need
                    // a running daemon — fall back to spawning the sidecar
                    // directly so the UI never gets stuck waiting.
                    spawn_daemon_fallback(&server);
                }
                // Protocol handshake: restart a daemon built in a different
                // build batch (stale binary left over from before an upgrade).
                reconcile_daemon_build(&server);
            } else {
                write_startup_log(&format!("dshboxd sidecar is missing: {}", server.display()));
            }
            if !cfg!(debug_assertions) {
                if let Ok(executable) = env::current_exe() {
                    let _ = install_tray_autostart(&executable);
                }
            }
            setup_tray(app.handle())?;
            if let Ok(config) = read_config() {
                if let Some(root) = config.runtime_directory {
                    repair_resources_on_startup(&root);
                }
            }
            write_startup_log("desktop setup completed");
            if env::args().any(|argument| argument == "--tray") || initial_container.is_some() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            if let Ok(paths) = task_paths() {
                let _ = app.state::<TaskManager>().restore(&paths);
            }
            let handle = app.handle().clone();
            thread::spawn(move || refresh_global_state(&handle));
            // Bridge daemon `/events` SSE stream to the Tauri event bus so
            // every state transition the daemon emits surfaces as a
            // `daemon://event` payload the frontend can subscribe to.
            spawn_event_subscriber(app.handle().clone());
            if let Some(id) = initial_container.clone() {
                let client = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = open_dsh_front_for_client(id, client).await {
                        write_startup_log(&format!("CLI container open failed: {error}"));
                    }
                });
            }

            // Graceful shutdown: on SIGTERM / SIGINT / Ctrl-C, persist the
            // local (toolchain-install) task state so the next launch can
            // resume it. The daemon owns container hosts, so there is nothing
            // to stop here.
            let shutdown_handle = app.handle().clone();
            ctrlc::set_handler(move || {
                write_startup_log("shutdown signal received; persisting task state");
                if let Ok(paths) = task_paths() {
                    let _ = shutdown_handle.state::<TaskManager>().persist(&paths);
                }
                write_startup_log("shutdown complete");
                std::process::exit(0);
            })
            .map_err(|error| format!("cannot register signal handler: {error}"))?;

            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::config::load_config,
            commands::config::save_runtime_directory,
            commands::config::save_language,
            commands::config::save_mirror_settings,
            get_server_service_status,
            get_daemon_status,
            restart_server_service,
            start_server_service,
            stop_server_service,
            commands::toolchains::detect_toolchains,
            commands::toolchains::save_toolchain_source,
            commands::toolchains::resolve_toolchain_command,
            commands::toolchains::run_toolchain_command,
            enqueue_toolchain_install,
            commands::versions::list_dsh_versions,
            commands::versions::upgrade_legacy_resources,
            enqueue_pull_template,
            enqueue_dsh_catalog_refresh,
            commands::versions::uninstall_dsh_version,
            commands::versions::list_installed_dsh_versions,
            create_dsh_container,
            add_dsh_container_profile,
            set_dsh_container_profile,
            commands::containers::list_dsh_containers,
            commands::state::get_resource_state,
            commands::state::get_container_details,
            commands::state::list_resource_states,
            commands::state::refresh_resource_state,
            commands::state::list_data_entries,
            commands::state::prune_orphaned_data,
            delete_dsh_container,
            enqueue_container_start,
            enqueue_container_stop,
            open_dsh_front,
            open_dsh_front_browser,
            enqueue_container_rebuild,
            enqueue_container_extension_add,
            enqueue_repository_extension_import,
            scan_container_workspace_extensions,
            enqueue_workspace_extension_import,
            enqueue_container_extension_copy,
            enqueue_repository_extension_export,
            enqueue_bundle_export,
            enqueue_bundle_import,
            enqueue_container_bundle_install,
            list_extension_bundles,
            create_extension_bundle,
            delete_extension_bundle,
            remove_repository_extension,
            list_repository_reference_counts,
            enqueue_plugin_export,
            remove_repository_plugin,
            image::enqueue_image_build,
            image::enqueue_image_commit_stub,
            image::enqueue_image_load_stub,
            image::preview_image_script_command,
            image::list_templates,
            image::read_template,
            image::import_template,
            image::export_template,
            image::remove_template,
            image::enqueue_template_container,
            list_tasks,
            cancel_task,
            delete_task,
            read_task_log,
            read_container_log,
            append_container_webview_log,
            retry_task
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .map_err(|error| format!("{error}"))
}
