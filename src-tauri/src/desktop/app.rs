use box_containers::{
    container_directory, scan_containers, CreateDshContainerRequest, DshContainer,
};
use box_dsh_versions::{
    installed_versions as installed_dsh_versions, version_directory as dsh_version_directory,
    DshVersion, DSH_REPOSITORY, DSH_TAGS_API,
};
use box_extensions::{
    detect_extension_kind, directory_size, extension_digest, read_bundles, remove_plugin_record,
    repository_root, scan_repository, scan_workspace_extensions, write_bundles, write_extension_record, write_repository_index,
    BundleEntry, ExtensionBundle, ExtensionKind, ExtensionRecord, RepositoryExtension,
};
use box_foundation::{
    is_safe_identifier, mirror_url, normalize_optional_url, normalize_runtime_directory,
    now_seconds, read_config, strip_verbatim_prefix, suppress_console_window, write_config,
    BoxConfig, BoxPaths,
};
use box_runtime::{remove_checkout, shallow_clone_with_cancel};
use box_scheduler::{run_queued, TaskContext, TaskManager, TaskNotifier, TaskRecord};
use box_server_core::{
    install_tray_autostart, install_user_service, restart_user_service, service_status,
    start_user_service, stop_user_service, ServiceStatus,
};
use box_state::{ResourceSnapshot, ResourceState, ResourceStateManager};
use box_toolchains::{is_known_toolchain, ToolchainStatus};
use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    ffi::OsString,
    io::{BufRead, BufReader},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Manager, WindowEvent};
use xz2::read::XzDecoder;

mod bundles;
mod commands;
mod containers;
mod extensions;
mod lifecycle;
mod services;
mod tasks;
mod toolchains;
mod versions;

pub(crate) use bundles::*;
pub(crate) use commands::versions::install_dsh_version_with_cancel;
pub(crate) use containers::*;
pub(crate) use extensions::*;
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

#[derive(Deserialize)]
pub(crate) struct GitHubTag {
    name: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub(crate) struct NodeRelease {
    version: String,
    files: Vec<String>,
}

pub(crate) struct ManagedHost {
    child: Child,
    url: String,
}

#[derive(Default)]
pub(crate) struct ContainerManager {
    running: Mutex<BTreeMap<String, ManagedHost>>,
}

pub(super) fn run() {
    if let Err(error) = run_inner() {
        write_startup_log(&format!("desktop startup failed: {error}"));
        panic!("DSH Box startup failed: {error}");
    }
}

fn run_inner() -> Result<(), String> {
    tauri::Builder::default()
        .manage(ContainerManager::default())
        .manage(TaskManager::default())
        .manage(ResourceStateManager::default())
        .setup(|app| {
            let resources = app
                .path()
                .resource_dir()
                .map_err(|error| error.to_string())?;
            write_startup_log(&format!("resource directory: {}", resources.display()));
            initialize_bundled_runtime(resources).map_err(|error| { write_startup_log(&format!("bundled runtime initialization failed: {error}")); error })?;
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
                if let Err(error) = install_user_service(&server) { write_startup_log(&format!("dshboxd service installation failed: {error}")); }
            } else {
                write_startup_log(&format!("dshboxd sidecar is missing: {}", server.display()));
            }
            if !cfg!(debug_assertions) {
                if let Ok(executable) = env::current_exe() {
                    let _ = install_tray_autostart(&executable);
                }
            }
            setup_tray(app.handle())?;
            write_startup_log("desktop setup completed");
            if env::args().any(|argument| argument == "--tray") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            if let Ok(paths) = task_paths() {
                let _ = app.state::<TaskManager>().restore(&paths);
            }
            let handle = app.handle().clone();
            thread::spawn(move || refresh_global_state(&handle));
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::config::load_config,
            commands::config::save_runtime_directory,
            commands::config::save_language,
            commands::config::save_mirror_settings,
            get_server_service_status,
            restart_server_service,
            start_server_service,
            stop_server_service,
            commands::toolchains::detect_toolchains,
            commands::toolchains::save_toolchain_source,
            commands::toolchains::resolve_toolchain_command,
            commands::toolchains::run_toolchain_command,
            enqueue_toolchain_install,
            commands::versions::list_dsh_versions,
            enqueue_dsh_version_install,
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
            enqueue_plugin_export,
            remove_repository_plugin,
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
