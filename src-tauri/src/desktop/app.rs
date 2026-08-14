use box_containers::{
    container_directory, scan_containers, CreateDshContainerRequest, DshContainer,
};
use box_dsh_versions::{
    installed_versions as installed_dsh_versions, version_directory as dsh_version_directory,
    DshVersion, DSH_REPOSITORY, DSH_TAGS_API,
};
use box_extensions::{
    detect_extension_kind, write_extension_record, ExtensionKind, ExtensionRecord,
};
use box_foundation::{
    is_safe_identifier, now_seconds, read_config, write_config, BoxConfig, BoxPaths,
};
use box_runtime::{remove_checkout, shallow_clone_with_cancel};
use box_scheduler::{TaskManager, TaskRecord};
use box_server_core::{
    install_tray_autostart, install_user_service, restart_user_service, service_status,
    start_user_service, stop_user_service, ServiceStatus,
};
use box_state::{ResourceSnapshot, ResourceState, ResourceStateManager};
use box_toolchains::{is_known_toolchain, ToolchainStatus};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use xz2::read::XzDecoder;

mod commands;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedToolchain {
    id: String,
    source: String,
    path: String,
    #[serde(default)]
    arguments: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundledRuntimeManifest {
    node_version: String,
    pnpm_version: String,
    node_entry: String,
    npm_entry: String,
    pnpm_entry: String,
}

struct BundledRuntime {
    node_version: String,
    npm_version: String,
    pnpm_version: String,
    node: PathBuf,
    npm: PathBuf,
    pnpm: PathBuf,
}

static BUNDLED_RUNTIME: OnceLock<BundledRuntime> = OnceLock::new();

#[allow(dead_code)]
const DEFAULT_NODE_VERSION: &str = "v24.11.1";
#[allow(dead_code)]
const DEFAULT_PNPM_VERSION: &str = "11.21.0";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolchainCommandRequest {
    id: String,
    args: Vec<String>,
    cwd: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddContainerExtensionRequest {
    id: String,
    profile: String,
    source: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportContainerPluginRequest {
    source_container_id: String,
    source_path: String,
    destination: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolchainCommandResult {
    path: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolchainInstallStatus {
    id: String,
    stage: String,
    log_path: String,
    lines: Vec<String>,
}

#[derive(Deserialize)]
struct GitHubTag {
    name: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct NodeRelease {
    version: String,
    files: Vec<String>,
}

struct ManagedHost {
    child: Child,
    url: String,
}

#[derive(Default)]
struct ContainerManager {
    running: Mutex<BTreeMap<String, ManagedHost>>,
}

#[derive(Clone)]
struct TaskContext {
    manager: TaskManager,
    app: tauri::AppHandle,
    id: String,
}

fn bundled_target() -> &'static str {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => "unsupported",
    }
}

fn bundled_server_path(resource_directory: &Path) -> PathBuf {
    let executable = if cfg!(windows) {
        "dshboxd.exe"
    } else {
        "dshboxd"
    };
    resource_directory
        .join("server")
        .join(bundled_target())
        .join(executable)
}

fn initialize_bundled_runtime(resource_directory: PathBuf) -> Result<(), String> {
    let root = resource_directory.join("runtime").join(bundled_target());
    let manifest: BundledRuntimeManifest = serde_json::from_str(
        &fs::read_to_string(root.join("runtime-manifest.json")).map_err(|_| {
            format!(
                "bundled runtime is missing for {}; reinstall DSH Box",
                bundled_target()
            )
        })?,
    )
    .map_err(|error| format!("cannot parse bundled runtime manifest: {error}"))?;
    let node = root.join(&manifest.node_entry);
    let npm = root.join(&manifest.npm_entry);
    let pnpm = root.join(&manifest.pnpm_entry);
    if !node.is_file() || !npm.is_file() || !pnpm.is_file() {
        return Err("bundled runtime is incomplete; reinstall DSH Box".to_owned());
    }
    let npm_version = Command::new(&node)
        .arg(&npm)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    BUNDLED_RUNTIME
        .set(BundledRuntime {
            node_version: manifest.node_version,
            npm_version,
            pnpm_version: manifest.pnpm_version,
            node,
            npm,
            pnpm,
        })
        .map_err(|_| "bundled runtime was initialized twice".to_owned())
}

#[tauri::command]
fn get_server_service_status() -> ServiceStatus {
    service_status()
}

#[tauri::command]
fn restart_server_service() -> Result<(), String> {
    restart_user_service()
}

#[tauri::command]
fn start_server_service() -> Result<(), String> {
    start_user_service()
}

#[tauri::command]
fn stop_server_service() -> Result<(), String> {
    stop_user_service()
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "tray-open", "Open dshbox", true, None::<&str>)?;
    let start = MenuItem::with_id(
        app,
        "tray-start-daemon",
        "Start dshboxd",
        true,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(app, "tray-stop-daemon", "Stop dshboxd", true, None::<&str>)?;
    let restart = MenuItem::with_id(
        app,
        "tray-restart-daemon",
        "Restart dshboxd",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit dshbox", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &start,
            &stop,
            &restart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("tray icon".to_owned()))?;
    TrayIconBuilder::with_id("dshbox-tray")
        .icon(icon)
        .tooltip("dshbox")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-open" => show_main_window(app),
            "tray-start-daemon" => {
                let _ = start_user_service();
            }
            "tray-stop-daemon" => {
                let _ = stop_user_service();
            }
            "tray-restart-daemon" => {
                let _ = restart_user_service();
            }
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn bundled_runtime() -> Result<&'static BundledRuntime, String> {
    BUNDLED_RUNTIME
        .get()
        .ok_or("bundled runtime is unavailable; restart DSH Box".to_owned())
}

#[allow(dead_code)]
fn managed_npm_script(root: &str) -> Option<PathBuf> {
    let candidate =
        PathBuf::from(root).join("tools/node/current/lib/node_modules/npm/bin/npm-cli.js");
    candidate.is_file().then_some(candidate)
}

fn detect_toolchain(id: &str, name: &str) -> ToolchainStatus {
    let version = bundled_runtime().ok().and_then(|runtime| match id {
        "node" => Some(runtime.node_version.clone()),
        "npm" => Some(runtime.npm_version.clone()),
        "pnpm" => Some(runtime.pnpm_version.clone()),
        _ => None,
    });
    ToolchainStatus {
        id: id.to_owned(),
        name: name.to_owned(),
        system_version: None,
        managed_version: version,
    }
}

fn scan_toolchains(_: &BoxConfig) -> Vec<ToolchainStatus> {
    [
        detect_toolchain("node", "Node.js"),
        detect_toolchain("npm", "npm"),
        detect_toolchain("pnpm", "pnpm"),
    ]
    .into()
}

fn task_records(manager: &TaskManager) -> Vec<TaskRecord> {
    manager.list().unwrap_or_default()
}

/// Rebuilds the read model from files and live process ownership. Failures are
/// intentionally non-fatal because operations must not be blocked by diagnostics.
fn refresh_global_state(app: &tauri::AppHandle) {
    let Ok(config) = read_config() else {
        return;
    };
    let mut containers = config
        .runtime_directory
        .as_deref()
        .and_then(|root| scan_containers(root).ok())
        .map(|items| items.into_values().collect::<Vec<_>>())
        .unwrap_or_default();
    if let Ok(mut running) = app.state::<ContainerManager>().running.lock() {
        for container in &mut containers {
            container.status = match running.get_mut(&container.id) {
                Some(host) => match host.child.try_wait() {
                    Ok(None) => "running".to_owned(),
                    Ok(Some(_)) | Err(_) => "stopped".to_owned(),
                },
                None => "stopped".to_owned(),
            };
        }
    }
    let versions = config
        .runtime_directory
        .as_deref()
        .and_then(|root| installed_dsh_versions(root).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|name| DshVersion {
            name,
            installed: true,
        })
        .collect();
    let state = app.state::<ResourceStateManager>();
    state.refresh_all(&config, scan_toolchains(&config), versions, containers);
    state.replace_tasks(task_records(&app.state::<TaskManager>()));
    if let Ok(paths) = BoxPaths::from_config(&config) {
        let _ = state.write_snapshot(&paths);
    }
}

fn resolve_toolchain(id: &str) -> Result<ResolvedToolchain, String> {
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

fn command_for_toolchain(toolchain: &ResolvedToolchain) -> Command {
    let mut command = Command::new(&toolchain.path);
    command.args(&toolchain.arguments);
    command
}

fn wait_for_process(
    child: &mut Child,
    task: Option<&TaskContext>,
    description: &str,
) -> Result<std::process::ExitStatus, String> {
    loop {
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("task cancelled while {description}"));
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn is_safe_version_name(version: &str) -> bool {
    is_safe_identifier(version)
}

fn fetch_dsh_tags() -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("cannot create GitHub client: {error}"))?;
    let response = client
        .get(DSH_TAGS_API)
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let tags: Vec<GitHubTag> = response
        .error_for_status()
        .map_err(|error| format!("GitHub tags request failed: {error}"))?
        .json()
        .map_err(|error| format!("cannot parse GitHub tags: {error}"))?;
    Ok(tags
        .into_iter()
        .map(|tag| tag.name)
        .filter(|name| is_safe_version_name(name))
        .collect())
}

fn dsh_catalog_path(root: &str) -> PathBuf {
    PathBuf::from(root).join("state/dsh-catalog.json")
}

fn read_dsh_catalog(root: &str) -> Vec<String> {
    fs::read_to_string(dsh_catalog_path(root))
        .ok()
        .and_then(|source| serde_json::from_str::<Vec<String>>(&source).ok())
        .unwrap_or_default()
}

fn refresh_dsh_catalog() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let tags = fetch_dsh_tags()?;
    let path = dsh_catalog_path(&root);
    fs::create_dir_all(path.parent().ok_or("invalid DSH catalog path")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string(&tags).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[allow(dead_code)]
fn node_platform() -> Result<String, String> {
    let os = match env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => return Err(format!("managed Node is not yet supported on {other}")),
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => return Err(format!("managed Node is not yet supported on {other}")),
    };
    Ok(format!("{os}-{arch}"))
}

#[allow(dead_code)]
fn install_managed_node(root: &str) -> Result<ToolchainInstallStatus, String> {
    let platform = node_platform()?;
    let releases: Vec<NodeRelease> = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .build()
        .map_err(|error| error.to_string())?
        .get("https://nodejs.org/dist/index.json")
        .send()
        .map_err(|error| format!("cannot download Node index: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .map_err(|error| format!("cannot parse Node index: {error}"))?;
    let release = releases
        .into_iter()
        .find(|release| {
            release.version == DEFAULT_NODE_VERSION && release.files.contains(&platform)
        })
        .ok_or_else(|| {
            format!("Box-compatible Node {DEFAULT_NODE_VERSION} is unavailable for {platform}")
        })?;
    let archive_url = format!(
        "https://nodejs.org/dist/{0}/node-{0}-{1}.tar.xz",
        release.version, platform
    );
    let logs = PathBuf::from(root).join("logs").join("toolchains");
    fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
    let log_path = logs.join(format!("node-{}.log", release.version));
    let mut lines = vec![
        format!("downloading Node {}", release.version),
        archive_url.clone(),
    ];
    let archive = reqwest::blocking::get(&archive_url)
        .map_err(|error| format!("cannot download Node: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .bytes()
        .map_err(|error| error.to_string())?;
    lines.push("extracting archive".to_owned());
    let tools = PathBuf::from(root).join("tools").join("node");
    fs::create_dir_all(&tools).map_err(|error| error.to_string())?;
    let temporary = tools.join(format!(".{}.tmp", release.version));
    if temporary.exists() {
        fs::remove_dir_all(&temporary).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&temporary).map_err(|error| error.to_string())?;
    tar::Archive::new(XzDecoder::new(archive.as_ref()))
        .unpack(&temporary)
        .map_err(|error| format!("cannot unpack Node: {error}"))?;
    let extracted = temporary.join(format!("node-{}-{}", release.version, platform));
    let current = tools.join("current");
    if current.exists() {
        fs::remove_dir_all(&current).map_err(|error| error.to_string())?;
    }
    fs::rename(&extracted, &current).map_err(|error| format!("cannot install Node: {error}"))?;
    let _ = fs::remove_dir_all(&temporary);
    lines.push(format!("installed Node {}", release.version));
    fs::write(&log_path, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())?;
    Ok(ToolchainInstallStatus {
        id: "node".to_owned(),
        stage: "ready".to_owned(),
        log_path: log_path.to_string_lossy().into_owned(),
        lines,
    })
}

#[allow(dead_code)]
fn install_managed_pnpm(root: &str) -> Result<ToolchainInstallStatus, String> {
    let config = read_config()?;
    if config.toolchain_sources.get("npm").map(String::as_str) == Some("managed")
        && managed_npm_script(root).is_none()
    {
        install_managed_node(root)?;
    }
    let npm = resolve_toolchain("npm")?;
    let logs = PathBuf::from(root).join("logs").join("toolchains");
    fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
    let log_path = logs.join(format!("pnpm-{DEFAULT_PNPM_VERSION}.log"));
    let prefix = PathBuf::from(root)
        .join("tools/pnpm")
        .join(DEFAULT_PNPM_VERSION);
    fs::create_dir_all(&prefix).map_err(|error| error.to_string())?;
    let mut lines = vec![format!(
        "installing pnpm@{DEFAULT_PNPM_VERSION} with {}",
        npm.path
    )];
    let arguments = vec![
        "install".to_owned(),
        "--prefix".to_owned(),
        prefix.to_string_lossy().into_owned(),
        "--cache".to_owned(),
        PathBuf::from(root)
            .join("store/npm-cache")
            .to_string_lossy()
            .into_owned(),
        format!("pnpm@{DEFAULT_PNPM_VERSION}"),
    ];
    let output = command_for_toolchain(&npm)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run npm: {error}"))?;
    lines.push(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    lines.push(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    if !output.status.success() {
        return Err(format!(
            "npm failed while installing pnpm; inspect {}",
            log_path.display()
        ));
    }
    lines.push(format!("installed pnpm@{DEFAULT_PNPM_VERSION}"));
    fs::write(&log_path, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())?;
    Ok(ToolchainInstallStatus {
        id: "pnpm".to_owned(),
        stage: "ready".to_owned(),
        log_path: log_path.to_string_lossy().into_owned(),
        lines,
    })
}
#[tauri::command]
fn start_toolchain_install(id: String) -> Result<ToolchainInstallStatus, String> {
    if !is_known_toolchain(&id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    Err("Node, npm, and pnpm are bundled with DSH Box; reinstall the application to repair the runtime".to_owned())
}

#[tauri::command]
fn create_dsh_container(
    request: CreateDshContainerRequest,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    let name = request.name.trim().to_owned();
    let version = request.version;
    let profile = request.profile.trim().to_owned();
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    if name.is_empty() || name.len() > 80 {
        return Err("container name must contain 1 to 80 characters".to_owned());
    }
    if !is_safe_identifier(&profile) {
        return Err("profile must use letters, numbers, dots, dashes, or underscores".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    if !dsh_version_directory(&root, &version).join(".git").is_dir() {
        return Err(format!("DSH version is not installed: {version}"));
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let id = format!("container-{timestamp}");
    let directory = PathBuf::from(&root).join("instances").join(&id);
    for name in ["profile", "logs", "state"] {
        fs::create_dir_all(directory.join(name))
            .map_err(|error| format!("cannot create container: {error}"))?;
    }
    create_profile_manifest(&directory, &profile)?;
    let metadata = serde_json::json!({ "id": id, "name": name, "version": version, "profile": profile, "source": dsh_version_directory(&root, &version) });
    fs::write(
        directory.join("container.json"),
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write container metadata: {error}"))?;
    let container = DshContainer {
        id,
        name,
        version,
        profile,
        directory: directory.to_string_lossy().into_owned(),
        status: "stopped".to_owned(),
    };
    refresh_global_state(&app);
    Ok(container)
}

fn create_profile_manifest(container_directory: &Path, profile: &str) -> Result<(), String> {
    let directory = container_directory.join("profile/profiles").join(profile);
    if directory.exists() {
        return Err(format!("profile already exists: {profile}"));
    }
    fs::create_dir_all(&directory).map_err(|error| format!("cannot create profile: {error}"))?;
    let manifest = serde_json::json!({
        "name": format!("dsh-profile-{profile}"),
        "private": true,
        "dependencies": {},
        "dsh": { "profile": { "bundles": profile_template_bundles(profile) } }
    });
    fs::write(
        directory.join("package.json"),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write profile manifest: {error}"))?;
    write_profile_support_files(&directory)
}

fn profile_template_bundles(profile: &str) -> Vec<&'static str> {
    match profile {
        "web" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        "headless" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        _ => vec!["@deepseek-ai/dsh-base"],
    }
}

fn write_profile_support_files(directory: &Path) -> Result<(), String> {
    let patch = directory.join("cordis.patch.yml");
    if !patch.exists() {
        fs::write(&patch, "# User overrides for this DSH profile.\n[]\n")
            .map_err(|error| format!("cannot write profile patch: {error}"))?;
    }
    let workspace = directory.join("pnpm-workspace.yaml");
    if !workspace.exists() {
        fs::write(
            &workspace,
            "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n",
        )
        .map_err(|error| format!("cannot write profile workspace: {error}"))?;
    }
    Ok(())
}

/// Repairs Box-created, empty named profiles from builds before profile templates were persisted.
fn repair_known_profile_template(container_directory: &Path, profile: &str) -> Result<(), String> {
    if !matches!(profile, "web" | "headless") {
        return Ok(());
    }
    let directory = container_directory.join("profile/profiles").join(profile);
    let manifest_path = directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("cannot read profile: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile: {error}"))?;
    let empty = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        manifest["dsh"]["profile"]["bundles"] =
            serde_json::json!(profile_template_bundles(profile));
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("cannot repair profile: {error}"))?;
    }
    write_profile_support_files(&directory)
}

/// Ensures every non-bundled DSH plugin selected by a profile has its declared runtime entry.
/// GitHub and tarball imports may contain TypeScript sources, so this prepares those sources
/// before the DSH loader attempts to import them.
fn preflight_profile_plugins(
    container_directory: &Path,
    profile: &str,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    let profile_directory = container_directory.join("profile/profiles").join(profile);
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(profile_directory.join("package.json"))
            .map_err(|error| format!("cannot read profile manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile manifest: {error}"))?;
    let bundles = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or("profile manifest has no dsh.profile.bundles")?;
    for bundle in bundles.iter().filter_map(serde_json::Value::as_str) {
        if bundle.starts_with("@deepseek-ai/") {
            continue;
        }
        let plugin_directory = profile_directory.join("node_modules").join(bundle);
        let plugin_manifest_path = plugin_directory.join("package.json");
        if !plugin_manifest_path.is_file() {
            return Err(format!(
                "profile plugin {bundle} is not installed; re-add it from Container details"
            ));
        }
        let plugin_manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&plugin_manifest_path)
                .map_err(|error| format!("cannot read plugin {bundle} manifest: {error}"))?,
        )
        .map_err(|error| format!("cannot parse plugin {bundle} manifest: {error}"))?;
        let Some(entry) = plugin_runtime_entry(&plugin_manifest) else {
            continue;
        };
        if plugin_directory.join(&entry).is_file() {
            continue;
        }
        if let Some(task) = task {
            task.update(format!("Preparing plugin {bundle}"), 32);
            task.log(&format!(
                "plugin {bundle} entry {entry} is missing; installing dependencies and building its source"
            ));
            prepare_plugin_source(&plugin_directory, bundle, &entry, task)?;
        } else {
            return Err(format!(
                "plugin {bundle} has no built entry {entry}; start it from DSH Box so it can be prepared"
            ));
        }
    }
    Ok(())
}

fn plugin_runtime_entry(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("main")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .pointer("/exports/./default")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

fn prepare_plugin_source(
    directory: &Path,
    name: &str,
    entry: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let frozen = if directory.join("pnpm-lock.yaml").is_file() {
        "--frozen-lockfile"
    } else {
        "--no-frozen-lockfile"
    };
    let mut install = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            directory.to_string_lossy().as_ref(),
            "install",
            frozen,
        ])
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .spawn()
        .map_err(|error| format!("cannot install dependencies for plugin {name}: {error}"))?;
    let status = wait_for_process(&mut install, Some(task), "installing plugin dependencies")?;
    if !status.success() {
        return Err(format!(
            "plugin {name} dependency installation exited with {status}"
        ));
    }
    if directory.join(entry).is_file() {
        return Ok(());
    }
    if plugin_has_script(directory, "build")? {
        task.update(format!("Building plugin {name}"), 38);
        let mut build = command_for_toolchain(&pnpm)
            .args([
                "--dir",
                directory.to_string_lossy().as_ref(),
                "run",
                "build",
            ])
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|error| format!("cannot build plugin {name}: {error}"))?;
        let status = wait_for_process(&mut build, Some(task), "building plugin")?;
        if !status.success() {
            return Err(format!("plugin {name} build exited with {status}"));
        }
    }
    if directory.join(entry).is_file() {
        Ok(())
    } else {
        Err(format!(
            "plugin {name} build completed but did not create its declared entry {entry}"
        ))
    }
}

fn plugin_has_script(directory: &Path, script: &str) -> Result<bool, String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(directory.join("package.json"))
            .map_err(|error| format!("cannot read plugin manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin manifest: {error}"))?;
    Ok(manifest
        .pointer(&format!("/scripts/{script}"))
        .and_then(serde_json::Value::as_str)
        .is_some())
}

#[tauri::command]
fn add_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    if !directory.join("container.json").is_file() {
        return Err(format!("container not found: {id}"));
    }
    create_profile_manifest(&directory, &profile)?;
    refresh_global_state(&app);
    app.state::<ResourceStateManager>()
        .snapshot()?
        .containers
        .into_iter()
        .find(|container| container.id == id)
        .ok_or("container disappeared after profile creation".to_owned())
}

#[tauri::command]
fn set_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata_path = directory.join("container.json");
    let mut metadata: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|error| format!("cannot read container: {error}"))?,
    )
    .map_err(|error| format!("cannot parse container: {error}"))?;
    if !directory
        .join("profile/profiles")
        .join(&profile)
        .join("package.json")
        .is_file()
    {
        return Err(format!("profile not found: {profile}"));
    }
    metadata["profile"] = serde_json::Value::String(profile);
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot save container: {error}"))?;
    refresh_global_state(&app);
    app.state::<ResourceStateManager>()
        .snapshot()?
        .containers
        .into_iter()
        .find(|container| container.id == id)
        .ok_or("container disappeared after profile update".to_owned())
}

#[tauri::command]
fn delete_dsh_container(
    id: String,
    manager: tauri::State<ContainerManager>,
    tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !is_safe_version_name(&id) {
        return Err("invalid container id".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(&id);
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = PathBuf::from(root).join("instances").join(&id);
    if !directory.is_dir() {
        return Err(format!("container not found: {id}"));
    }
    fs::remove_dir_all(directory).map_err(|error| format!("cannot remove container: {error}"))?;
    refresh_global_state(&app);
    Ok(())
}

fn task_paths() -> Result<BoxPaths, String> {
    let config = read_config()?;
    BoxPaths::from_config(&config)
}

fn persist_tasks(manager: &TaskManager) -> Result<(), String> {
    manager.persist(&task_paths()?)
}
fn queue_task(
    manager: &TaskManager,
    app: &tauri::AppHandle,
    kind: &str,
    resource_keys: Vec<String>,
    params: serde_json::Value,
) -> Result<TaskRecord, String> {
    let task = manager.enqueue(&task_paths()?, kind, resource_keys, params)?;
    let state = app.state::<ResourceStateManager>();
    state.apply_task_update(task.clone());
    if let Ok(config) = read_config() {
        if let Ok(paths) = BoxPaths::from_config(&config) {
            let _ = state.write_snapshot(&paths);
        }
    }
    app.emit("task://created", &task)
        .map_err(|error| error.to_string())?;
    Ok(task)
}

fn append_task_log(manager: &TaskManager, app: &tauri::AppHandle, task_id: &str, message: &str) {
    let task = manager.task(task_id).ok();
    if let Some(task) = task {
        let line = format!("[{}] {message}\n", now_seconds());
        let _ = fs::OpenOptions::new()
            .append(true)
            .open(&task.log_path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        let _ = app.emit(
            "task://log",
            serde_json::json!({ "taskId": task_id, "line": line.trim_end() }),
        );
    }
}

fn emit_task_update(manager: &TaskManager, app: &tauri::AppHandle, task_id: &str) {
    if let Ok(task) = manager.task(task_id) {
        let _ = app.emit("task://updated", task);
    }
    let _ = persist_tasks(manager);
    let state = app.state::<ResourceStateManager>();
    if let Ok(task) = manager.task(task_id) {
        state.apply_task_update(task);
    }
    if let Ok(config) = read_config() {
        if let Ok(paths) = BoxPaths::from_config(&config) {
            let _ = state.write_snapshot(&paths);
        }
    }
}

fn ensure_resource_idle(manager: &TaskManager, resource: &str) -> Result<(), String> {
    if !manager.resource_idle(resource)? {
        Err(format!("resource is busy: {resource}"))
    } else {
        Ok(())
    }
}

impl TaskContext {
    fn cancelled(&self) -> bool {
        self.manager
            .task(&self.id)
            .map(|task| task.cancel_requested)
            .unwrap_or(true)
    }

    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancelled() {
            Err("task cancelled".to_owned())
        } else {
            Ok(())
        }
    }

    fn update(&self, stage: impl Into<String>, progress: u8) {
        if let Ok(paths) = task_paths() {
            let _ = self.manager.update(&paths, &self.id, stage, progress);
        }
        emit_task_update(&self.manager, &self.app, &self.id);
    }

    fn log(&self, message: &str) {
        append_task_log(&self.manager, &self.app, &self.id, message);
    }
}

fn run_queued_task(
    manager: TaskManager,
    app: tauri::AppHandle,
    task_id: String,
    work: impl FnOnce(TaskContext) -> Result<(), String> + Send + 'static,
) {
    thread::spawn(move || {
        loop {
            let Ok(paths) = task_paths() else {
                return;
            };
            match manager.try_start(&paths, &task_id) {
                Ok(Some(task)) if task.status == "cancelled" => {
                    append_task_log(&manager, &app, &task_id, "cancelled before execution");
                    emit_task_update(&manager, &app, &task_id);
                    return;
                }
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => return,
            }
        }
        append_task_log(&manager, &app, &task_id, "worker started");
        emit_task_update(&manager, &app, &task_id);
        let context = TaskContext {
            manager: manager.clone(),
            app: app.clone(),
            id: task_id.clone(),
        };
        let result = work(context);
        let final_task = task_paths()
            .ok()
            .and_then(|paths| manager.finish(&paths, &task_id, &result).ok());
        if let Some(task) = &final_task {
            let _ = app.emit("task://finished", task);
        }
        let final_status = final_task
            .map(|task| task.status)
            .unwrap_or_else(|| "failed".to_owned());
        append_task_log(
            &manager,
            &app,
            &task_id,
            match final_status.as_str() {
                "succeeded" => "completed",
                "cancelled" => "cancelled after the active operation returned",
                _ => "failed; inspect the error summary",
            },
        );
        refresh_global_state(&app);
    });
}

#[tauri::command]
fn enqueue_toolchain_install(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "toolchain-install",
        vec![format!("toolchain:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Preparing toolchain installation", 5);
        task.check_cancelled()?;
        task.log("starting toolchain installer");
        let result = start_toolchain_install(id).map(|_| ());
        task.update("Finalizing toolchain installation", 95);
        result
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_dsh_version_install(
    version: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "dsh-version-install",
        vec![format!("runtime:{version}")],
        serde_json::json!({ "version": version }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Cloning DSH source", 10);
        task.check_cancelled()?;
        task.log("starting DSH clone");
        let cancellation = task.clone();
        let result = commands::versions::install_dsh_version_with_cancel(version, move || {
            cancellation.cancelled()
        })
        .map(|_| ());
        task.update("Finalizing DSH runtime", 95);
        result
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_dsh_catalog_refresh(
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "dsh-catalog-refresh",
        vec!["catalog:dsh".to_owned()],
        serde_json::json!({}),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Fetching DSH versions", 20);
        task.log("requesting DSH version catalog from GitHub");
        task.check_cancelled()?;
        refresh_dsh_catalog()?;
        task.check_cancelled()?;
        task.update("Version catalog refreshed", 95);
        Ok(())
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_container_start(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "container-start",
        vec![format!("container:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Starting DSH host", 10);
        task.check_cancelled()?;
        start_dsh_container_with_task(id, work_app.state::<ContainerManager>(), Some(&task))
            .map(|_| ())
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_container_stop(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "container-stop",
        vec![format!("container:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Stopping DSH host", 30);
        task.check_cancelled()?;
        stop_dsh_container(id, work_app.state::<ContainerManager>())
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_container_rebuild(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let metadata = fs::read_to_string(container_directory(&root, &id).join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let version = serde_json::from_str::<serde_json::Value>(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?["version"]
        .as_str()
        .ok_or("container has no version")?
        .to_owned();
    let task = queue_task(
        &manager,
        &app,
        "container-rebuild",
        vec![format!("container:{id}"), format!("runtime:{version}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Rebuilding DSH runtime", 10);
        task.check_cancelled()?;
        rebuild_dsh_container_with_task(id, work_app.state::<ContainerManager>(), Some(&task))
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_container_extension_add(
    request: AddContainerExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_identifier(&request.profile) {
        return Err("invalid container or profile name".to_owned());
    }
    let source = request.source.trim().to_owned();
    if source.is_empty() {
        return Err("extension source is required".to_owned());
    }
    let task = queue_task(
        &manager,
        &app,
        "container-extension-add",
        vec![format!("container:{}", request.id)],
        serde_json::json!({ "id": request.id, "profile": request.profile, "source": source }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        install_container_extension(request, &task)
    });
    Ok(task)
}

#[tauri::command]
fn enqueue_plugin_export(
    request: ExportContainerPluginRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.source_container_id)
        || request.source_path.trim().is_empty()
        || request.destination.trim().is_empty()
    {
        return Err("invalid plugin export request".to_owned());
    }
    let task = queue_task(
        &manager,
        &app,
        "plugin-export",
        vec![format!("container:{}", request.source_container_id)],
        serde_json::json!({ "sourceContainerId": request.source_container_id, "sourcePath": request.source_path, "destination": request.destination }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        export_repository_plugin(request, &task)
    });
    Ok(task)
}

fn export_repository_plugin(
    request: ExportContainerPluginRequest,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let instance_root = PathBuf::from(&root)
        .join("instances")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let source = PathBuf::from(&request.source_path)
        .canonicalize()
        .map_err(|error| format!("cannot find plugin source: {error}"))?;
    if !source.starts_with(&instance_root) || !source.join("package.json").is_file() {
        return Err("plugin source is not a DSH Box managed plugin".to_owned());
    }
    let destination = PathBuf::from(&request.destination);
    if destination.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("plugin export destination must end in .tar.gz".to_owned());
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("plugin export has no parent directory")?,
    )
    .map_err(|error| error.to_string())?;
    task.update("Packaging plugin tarball", 30);
    task.log(&format!(
        "exporting {} to {}",
        source.display(),
        destination.display()
    ));
    let output = fs::File::create(&destination)
        .map_err(|error| format!("cannot create plugin tarball: {error}"))?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_plugin_archive(&mut archive, &source, &source, Path::new("extension"))?;
    archive.finish().map_err(|error| error.to_string())?;
    task.check_cancelled()?;
    task.update("Plugin tarball exported", 95);
    Ok(())
}

fn append_plugin_archive(
    archive: &mut tar::Builder<GzEncoder<fs::File>>,
    root: &Path,
    directory: &Path,
    target: &Path,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        let path = entry.path();
        let output = target.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            archive
                .append_dir(&output, &path)
                .map_err(|error| error.to_string())?;
            append_plugin_archive(archive, root, &path, &output)?;
        } else if kind.is_file() {
            archive
                .append_path_with_name(&path, &output)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn install_container_extension(
    request: AddContainerExtensionRequest,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = scan_containers(&root)?
        .remove(&request.id)
        .ok_or("container not found")?;
    let profile_dir = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(&request.profile);
    if !profile_dir.join("package.json").is_file() {
        return Err(format!("profile not found: {}", request.profile));
    }
    let source = request.source.trim();
    let source_kind = if source.starts_with("https://github.com/") {
        "github"
    } else if Path::new(source).is_dir() {
        "repository"
    } else {
        "tarball"
    };
    let staging = PathBuf::from(&container.directory)
        .join("extensions/staging")
        .join(&task.id);
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    task.update("Importing extension source", 15);
    let extracted = if source_kind == "github" {
        let destination = staging.join("source");
        task.log(&format!("cloning GitHub repository {source}"));
        let cancelled = task.clone();
        shallow_clone_with_cancel(source, &destination, None, move || cancelled.cancelled())?;
        destination
    } else if source_kind == "repository" {
        let destination = staging.join("source");
        task.log(&format!("copying plugin from DSH Box repository {source}"));
        copy_extension_source(Path::new(source), &destination)?;
        destination
    } else {
        let archive = PathBuf::from(source);
        if !archive.is_file() {
            return Err("tarball source must be an existing local file".to_owned());
        }
        task.log(&format!("extracting tarball {}", archive.display()));
        let destination = staging.join("source");
        fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
        extract_extension_tarball(&archive, &destination)?;
        archive_content_root(&destination)?
    };
    task.check_cancelled()?;
    task.update("Detecting extension type", 40);
    let kind = detect_extension_kind(&extracted)?;
    match kind {
        ExtensionKind::Skill => {
            install_container_skill(&container, source_kind, source, extracted, task)
        }
        ExtensionKind::Plugin => install_container_plugin(
            &container,
            &request.profile,
            source_kind,
            source,
            extracted,
            task,
        ),
    }?;
    let _ = fs::remove_dir_all(staging);
    task.update("Refreshing container extensions", 95);
    Ok(())
}

fn copy_extension_source(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        let target = destination.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            copy_extension_source(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn extract_extension_tarball(archive: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|error| format!("cannot open tarball: {error}"))?;
    let name = archive.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        unpack_tar(tar::Archive::new(GzDecoder::new(file)), destination)
    } else if name.ends_with(".tar.xz") {
        unpack_tar(tar::Archive::new(XzDecoder::new(file)), destination)
    } else if name.ends_with(".tar") {
        unpack_tar(tar::Archive::new(file), destination)
    } else {
        Err("supported archives are .tar, .tar.gz, .tgz, and .tar.xz".to_owned())
    }
}

fn unpack_tar<R: std::io::Read>(
    mut archive: tar::Archive<R>,
    destination: &Path,
) -> Result<(), String> {
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot read tarball: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("cannot read tarball entry: {error}"))?;
        let path = entry.path().map_err(|error| error.to_string())?;
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("tarball contains an unsafe path".to_owned());
        }
        if !entry
            .unpack_in(destination)
            .map_err(|error| format!("cannot extract tarball: {error}"))?
        {
            return Err("tarball entry escaped the destination".to_owned());
        }
    }
    Ok(())
}

fn archive_content_root(destination: &Path) -> Result<PathBuf, String> {
    if destination.join("SKILL.md").is_file() || destination.join("package.json").is_file() {
        return Ok(destination.to_path_buf());
    }
    let entries = fs::read_dir(destination)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    if entries.len() == 1 && entries[0].path().is_dir() {
        Ok(entries[0].path())
    } else {
        Err("tarball must contain one extension directory".to_owned())
    }
}

fn install_container_skill(
    container: &DshContainer,
    source_kind: &str,
    source: &str,
    extracted: PathBuf,
    task: &TaskContext,
) -> Result<(), String> {
    let name = skill_name(&extracted.join("SKILL.md"))?;
    if !is_safe_identifier(&name) {
        return Err(
            "skill name must use letters, numbers, dots, dashes, or underscores".to_owned(),
        );
    }
    let destination = PathBuf::from(&container.directory)
        .join("profile/skills")
        .join(&name);
    if destination.exists() {
        return Err(format!("skill already exists: {name}"));
    }
    task.update("Installing container skill", 65);
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("skill destination has no parent")?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(&extracted, &destination)
        .map_err(|error| format!("cannot install skill: {error}"))?;
    write_extension_record(
        container,
        ExtensionRecord {
            kind: ExtensionKind::Skill,
            name,
            source_kind: source_kind.to_owned(),
            source: source.to_owned(),
            profile: None,
            path: destination.to_string_lossy().into_owned(),
            installed_at: now_seconds(),
        },
    )
}

fn skill_name(path: &Path) -> Result<String, String> {
    let content =
        fs::read_to_string(path).map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    let name = content
        .lines()
        .find_map(|line| line.strip_prefix("name:").map(str::trim))
        .ok_or("skill frontmatter has no name")?;
    Ok(name.trim_matches(['\'', '"']).to_owned())
}

fn install_container_plugin(
    container: &DshContainer,
    profile: &str,
    source_kind: &str,
    source: &str,
    extracted: PathBuf,
    task: &TaskContext,
) -> Result<(), String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(extracted.join("package.json"))
            .map_err(|error| format!("cannot read plugin package.json: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin package.json: {error}"))?;
    let name = manifest["name"]
        .as_str()
        .ok_or("plugin package.json has no name")?
        .to_owned();
    let source_directory = PathBuf::from(&container.directory)
        .join("extensions/plugins")
        .join(&task.id)
        .join("source");
    fs::create_dir_all(
        source_directory
            .parent()
            .ok_or("plugin destination has no parent")?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(&extracted, &source_directory)
        .map_err(|error| format!("cannot store plugin source: {error}"))?;
    task.update("Installing DSH plugin", 60);
    task.log(&format!("adding plugin {name} to profile {profile}"));
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let dsh_source = dsh_version_directory(&root, &container.version);
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let mut child = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            dsh_source.to_string_lossy().as_ref(),
            "dsh",
            "plugin",
            "--profile",
            profile,
            "add",
            source_directory.to_string_lossy().as_ref(),
        ])
        .env(
            "DSH_HOME",
            PathBuf::from(&container.directory).join("profile"),
        )
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| format!("cannot start plugin install: {error}"))?;
    let status = wait_for_process(&mut child, Some(task), "installing plugin")?;
    if !status.success() {
        return Err(format!("dsh plugin add exited with {status}"));
    }
    write_extension_record(
        container,
        ExtensionRecord {
            kind: ExtensionKind::Plugin,
            name,
            source_kind: source_kind.to_owned(),
            source: source.to_owned(),
            profile: Some(profile.to_owned()),
            path: source_directory.to_string_lossy().into_owned(),
            installed_at: now_seconds(),
        },
    )
}

#[tauri::command]
fn list_tasks(resources: tauri::State<ResourceStateManager>) -> Result<Vec<TaskRecord>, String> {
    Ok(resources.snapshot()?.tasks)
}

#[tauri::command]
fn cancel_task(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    manager.request_cancel(&task_paths()?, &id)?;
    emit_task_update(&manager, &app, &id);
    Ok(())
}

#[tauri::command]
fn read_task_log(
    id: String,
    resources: tauri::State<ResourceStateManager>,
) -> Result<String, String> {
    let task = resources
        .snapshot()?
        .tasks
        .into_iter()
        .find(|task| task.id == id)
        .ok_or("task not found")?;
    fs::read_to_string(task.log_path).map_err(|error| format!("cannot read task log: {error}"))
}

#[tauri::command]
fn retry_task(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let previous = manager.task(&id)?;
    match previous.kind.as_str() {
        "toolchain-install" => enqueue_toolchain_install(
            previous.params["id"]
                .as_str()
                .ok_or("task has no toolchain id")?
                .to_owned(),
            manager,
            app,
        ),
        "dsh-version-install" => enqueue_dsh_version_install(
            previous.params["version"]
                .as_str()
                .ok_or("task has no DSH version")?
                .to_owned(),
            manager,
            app,
        ),
        "dsh-catalog-refresh" => enqueue_dsh_catalog_refresh(manager, app),
        "container-start" => enqueue_container_start(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-stop" => enqueue_container_stop(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-rebuild" => enqueue_container_rebuild(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-extension-add" => enqueue_container_extension_add(
            AddContainerExtensionRequest {
                id: previous.params["id"]
                    .as_str()
                    .ok_or("task has no container id")?
                    .to_owned(),
                profile: previous.params["profile"]
                    .as_str()
                    .ok_or("task has no profile")?
                    .to_owned(),
                source: previous.params["source"]
                    .as_str()
                    .ok_or("task has no extension source")?
                    .to_owned(),
            },
            manager,
            app,
        ),
        _ => Err("this task type cannot be retried".to_owned()),
    }
}

fn start_dsh_container_with_task(
    id: String,
    manager: tauri::State<ContainerManager>,
    task: Option<&TaskContext>,
) -> Result<String, String> {
    if !is_safe_version_name(&id) {
        return Err("invalid container id".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    let version = value["version"]
        .as_str()
        .ok_or("container has no version")?;
    let profile = value["profile"].as_str().unwrap_or("web");
    repair_known_profile_template(&directory, profile)?;
    let source = dsh_version_directory(&root, version);
    if !source.join("package.json").is_file() {
        return Err("DSH source is incomplete".to_owned());
    }
    {
        let mut running = manager
            .running
            .lock()
            .map_err(|_| "container manager lock failed")?;
        if let Some(host) = running.get_mut(&id) {
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
    let log = fs::File::create(directory.join("logs").join("host.log"))
        .map_err(|error| format!("cannot create host log: {error}"))?;
    if !source
        .join("apps")
        .join("web")
        .join("dist")
        .join("index.html")
        .is_file()
    {
        if let Some(task) = task {
            task.update("Building DSH frontend", 40);
            task.log("building DSH frontend");
        }
        let mut build = command_for_toolchain(&pnpm)
            .args(["--dir", source.to_string_lossy().as_ref(), "run", "build"])
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .spawn()
            .map_err(|error| format!("cannot build DSH before launch: {error}"))?;
        let status = wait_for_process(&mut build, task, "building DSH frontend")?;
        if !status.success() {
            return Err(format!(
                "DSH build failed; inspect {}",
                directory.join("logs").join("host.log").display()
            ));
        }
        if let Some(task) = task {
            task.check_cancelled()?;
        }
    }
    if let Some(task) = task {
        task.update("Launching DSH host", 75);
        task.log("launching DSH host");
    }
    let mut child = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            source.to_string_lossy().as_ref(),
            "dsh",
            "--profile",
            profile,
            "--patch",
            patch.to_string_lossy().as_ref(),
        ])
        .env("DSH_HOME", directory.join("profile"))
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| format!("cannot start DSH host: {error}"))?;
    let url = format!("http://127.0.0.1:{port}");
    let ready = (0..80).any(|_| {
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        let available = reqwest::blocking::get(&url)
            .map(|response| response.status().is_success())
            .unwrap_or(false);
        if !available {
            thread::sleep(Duration::from_millis(250));
        }
        available
    });
    if !ready {
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            return Err("task cancelled while waiting for DSH host".to_owned());
        }
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "DSH host did not become ready; inspect {}",
            directory.join("logs").join("host.log").display()
        ));
    }
    manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .insert(
            id,
            ManagedHost {
                child,
                url: url.clone(),
            },
        );
    if let Some(task) = task {
        task.update("DSH host is ready", 95);
    }
    Ok(url)
}

#[tauri::command]
fn stop_dsh_container(id: String, manager: tauri::State<ContainerManager>) -> Result<(), String> {
    let host = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(&id);
    if let Some(mut host) = host {
        host.child
            .kill()
            .map_err(|error| format!("cannot stop DSH host: {error}"))?;
        let _ = host.child.wait();
    }
    Ok(())
}

#[tauri::command]
fn open_dsh_front(
    id: String,
    manager: tauri::State<ContainerManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let url = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .get(&id)
        .map(|host| host.url.clone())
        .ok_or("DSH host is not running")?;
    let label = format!("dsh-front-{id}");
    if let Some(window) = app.get_webview_window(&label) {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let target = url
        .parse()
        .map_err(|error| format!("invalid DSH URL: {error}"))?;
    WebviewWindowBuilder::new(&app, label, WebviewUrl::External(target))
        .title("DSH")
        .build()
        .map_err(|error| format!("cannot open DSH front: {error}"))?;
    Ok(())
}

fn rebuild_dsh_container_with_task(
    id: String,
    manager: tauri::State<ContainerManager>,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    if let Some(task) = task {
        task.update("Stopping DSH host", 20);
    }
    stop_dsh_container(id.clone(), manager.clone())?;
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
    let log = fs::File::create(&log_path)
        .map_err(|error| format!("cannot create rebuild log: {error}"))?;
    for (index, args) in [
        ["--dir", source.to_string_lossy().as_ref(), "install"],
        ["--dir", source.to_string_lossy().as_ref(), "build"],
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
        let mut command = command_for_toolchain(&pnpm)
            .args(args)
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .spawn()
            .map_err(|error| format!("cannot run pnpm: {error}"))?;
        let status = wait_for_process(&mut command, task, "running pnpm")?;
        if !status.success() {
            return Err(format!(
                "DSH rebuild failed; inspect {}",
                log_path.display()
            ));
        }
    }
    start_dsh_container_with_task(id, manager, task).map(|_| ())
}

pub(super) fn run() {
    tauri::Builder::default()
        .manage(ContainerManager::default())
        .manage(TaskManager::default())
        .manage(ResourceStateManager::default())
        .setup(|app| {
            let resources = app
                .path()
                .resource_dir()
                .map_err(|error| error.to_string())?;
            initialize_bundled_runtime(resources)?;
            let server = bundled_server_path(
                &app.path()
                    .resource_dir()
                    .map_err(|error| error.to_string())?,
            );
            if server.is_file() {
                let _ = install_user_service(&server);
            }
            if !cfg!(debug_assertions) {
                if let Ok(executable) = env::current_exe() {
                    let _ = install_tray_autostart(&executable);
                }
            }
            setup_tray(app.handle())?;
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
            enqueue_container_rebuild,
            enqueue_container_extension_add,
            enqueue_plugin_export,
            list_tasks,
            cancel_task,
            read_task_log,
            retry_task
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run DSH Box");
}
