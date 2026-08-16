//! Daemon state: the single process that owns the task queue, the bundled
//! Node/pnpm runtime, the running-container registry, and the resource
//! snapshot. CLI and desktop clients talk to this state via RPC; they never
//! construct their own copies.

use box_foundation::{read_config, strip_verbatim_prefix, BoxConfig, BoxPaths, BoxResult};
use box_scheduler::{TaskManager, TaskNotifier};
use box_state::ResourceStateManager;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

/// A managed host process (container server) owned by the daemon.
/// Mirrors the desktop's `ManagedHost`; clients never hold these.
pub(crate) struct ManagedHost {
    pub(crate) child: Child,
    pub(crate) url: String,
    pub(crate) tree: Arc<Mutex<Vec<u32>>>,
}

/// Registry of containers running inside this daemon process. The daemon
/// becomes the single owner of running containers once the desktop
/// migrates to thin-client mode.
#[derive(Default)]
pub(crate) struct ContainerManager {
    pub(crate) running: Mutex<BTreeMap<String, ManagedHost>>,
}

/// Bundled Node/npm/pnpm resolved from the resource directory.
pub(crate) struct BundledRuntime {
    pub(crate) node_version: String,
    pub(crate) npm_version: String,
    pub(crate) pnpm_version: String,
    pub(crate) node: PathBuf,
    pub(crate) npm: PathBuf,
    pub(crate) pnpm: PathBuf,
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

static BUNDLED_RUNTIME: OnceLock<BundledRuntime> = OnceLock::new();

pub(crate) fn bundled_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => "unsupported",
    }
}

/// Locate the resource directory (parent of `runtime/<target>/`) that
/// ships the bundled Node/pnpm. The daemon binary is deployed at
/// `resources/server/<target>/dshboxd`, so the resource root is two
/// directories up from the executable; developer builds fall back to
/// `src-tauri/resources` relative to this crate.
fn resource_directory() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("DSHBOX_RESOURCE_DIR") {
        return Ok(PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe() {
        // exe: resources/server/<target>/dshboxd
        if let Some(parent) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            if parent.join("runtime").is_dir() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources");
    if dev.join("runtime").is_dir() {
        return Ok(dev);
    }
    Err("bundled runtime is missing; reinstall DSH Box or set DSHBOX_RESOURCE_DIR".to_owned())
}

/// Initialize the bundled runtime exactly once. Called during daemon
/// startup so every task worker can resolve node/npm/pnpm.
pub(crate) fn initialize_bundled_runtime() -> Result<(), String> {
    let root = resource_directory()?.join("runtime").join(bundled_target());
    let manifest: BundledRuntimeManifest = serde_json::from_str(
        &std::fs::read_to_string(root.join("runtime-manifest.json")).map_err(|_| {
            format!(
                "bundled runtime is missing for {}; reinstall DSH Box",
                bundled_target()
            )
        })?,
    )
    .map_err(|error| format!("cannot parse bundled runtime manifest: {error}"))?;
    let plain = |entry: &str| {
        PathBuf::from(strip_verbatim_prefix(&root.join(entry).to_string_lossy()))
    };
    let node = plain(&manifest.node_entry);
    let npm = plain(&manifest.npm_entry);
    let pnpm = plain(&manifest.pnpm_entry);
    if !node.is_file() || !npm.is_file() || !pnpm.is_file() {
        return Err("bundled runtime is incomplete; reinstall DSH Box".to_owned());
    }
    let mut version_probe = Command::new(&node);
    box_foundation::suppress_console_window(&mut version_probe);
    let npm_version = version_probe
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

pub(crate) fn bundled_runtime() -> Result<&'static BundledRuntime, String> {
    BUNDLED_RUNTIME
        .get()
        .ok_or("bundled runtime is unavailable; start dshboxd first".to_owned())
}

/// Vendor the bundled Cordis plugin tree (`resources/plugins/<target>/`,
/// carrying `@deepseek-ai/dsh-box-context`) into
/// `<runtimeDirectory>/plugins/`, mirroring the desktop's
/// `initialize_bundled_plugins`. Without this the container-start path
/// (`ensure_bundled_context_plugin`) finds no vendored tree, the profile
/// symlink is skipped, and the DSH host dies with
/// `Cannot find package '@deepseek-ai/dsh-box-context'` — exactly the
/// failure mode every CLI-driven container start used to hit because the
/// daemon never ran the desktop-only copy.
///
/// Digest-idempotent: the manifest body is hashed and compared against
/// `BoxConfig.plugins_manifest_digest`, so repeat starts are no-ops. Defers
/// silently while no runtime directory is configured; the
/// `save_runtime_directory` RPC re-runs this so onboarding picks it up.
pub(crate) fn initialize_bundled_plugins() -> Result<(), String> {
    use box_foundation::{read_config, write_config};
    use std::hash::{Hash, Hasher};

    let resource_plugins = match resource_directory() {
        Ok(root) => root.join("plugins").join(bundled_target()),
        Err(error) => {
            eprintln!("dshboxd: bundled plugins skipped: {error}");
            return Ok(());
        }
    };
    let manifest_path = resource_plugins.join("plugins-manifest.json");
    if !manifest_path.is_file() {
        // Developer build that skipped the plugin bundler; the container
        // start path tolerates the missing tree.
        eprintln!(
            "dshboxd: bundled plugins skipped: {} not found",
            manifest_path.display()
        );
        return Ok(());
    }
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    manifest_bytes.hash(&mut hasher);
    bundled_target().hash(&mut hasher);
    let digest = format!("{:016x}", hasher.finish());

    let config = read_config()?;
    if config.plugins_manifest_digest.as_deref() == Some(digest.as_str()) {
        return Ok(());
    }
    let Some(runtime_directory) = config.runtime_directory.as_ref() else {
        return Ok(());
    };
    let runtime_root = PathBuf::from(runtime_directory);
    if !runtime_root.is_dir() {
        return Ok(());
    }

    let cache_root = runtime_root.join("plugins");
    if cache_root.exists() {
        std::fs::remove_dir_all(&cache_root)
            .map_err(|error| format!("cannot clean {}: {error}", cache_root.display()))?;
    }
    copy_dir_recursive(&resource_plugins, &cache_root)
        .map_err(|error| format!("cannot vendor bundled plugins: {error}"))?;

    let mut updated = config;
    updated.plugins_manifest_digest = Some(digest);
    write_config(&updated)?;
    eprintln!("dshboxd vendored bundled plugins into {}", cache_root.display());
    Ok(())
}

/// Recursive directory copy for the bundled plugin tree (files and
/// directories only; symlinks are not expected inside the resource).
fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Everything the daemon owns, shared with every connection thread.
pub(crate) struct DaemonState {
    pub(crate) manager: TaskManager,
    /// Box paths derived from the persisted config. Held behind a lock so
    /// `save_runtime_directory` can move the storage root while the daemon
    /// stays up (a daemon started before onboarding must adopt the newly
    /// chosen runtime directory without a restart).
    pub(crate) paths: RwLock<BoxPaths>,
    /// Container registry shared with task workers: long-running container
    /// starts run on worker threads, so the registry is behind an `Arc`.
    pub(crate) containers: Arc<ContainerManager>,
    /// Reserved for upcoming resource RPCs (toolchain download state).
    #[allow(dead_code)]
    pub(crate) resources: ResourceStateManager,
}

impl DaemonState {
    pub(crate) fn load() -> Result<Self, String> {
        let config = read_config().map_err(|error| format!("cannot read config: {error}"))?;
        let paths = BoxPaths::from_config(&config)
            .map_err(|error| format!("cannot derive box paths: {error}"))?;
        let manager = TaskManager::default();
        let _ = manager.restore(&paths);
        Ok(Self {
            manager,
            paths: RwLock::new(paths),
            containers: Arc::new(ContainerManager::default()),
            resources: ResourceStateManager::default(),
        })
    }

    /// Adopt a freshly-persisted config: re-derives box paths so the task
    /// queue and every later worker resolves the new storage root.
    pub(crate) fn adopt_config(&self, config: &BoxConfig) -> BoxResult<()> {
        let paths = BoxPaths::from_config(config)?;
        *self
            .paths
            .write()
            .map_err(|_| "daemon paths lock failed".to_owned())? = paths;
        Ok(())
    }
}

/// Task notifier for daemon-run tasks: persists progress to the task log
/// file and refreshes the resource snapshot. There is no event bus to emit
/// to; clients poll `task_status`/`list_tasks` instead.
pub(crate) struct DaemonNotifier {
    manager: TaskManager,
    resources: ResourceStateManager,
    paths: BoxPaths,
}

impl DaemonNotifier {
    pub(crate) fn from_paths(manager: TaskManager, paths: BoxPaths) -> Self {
        Self {
            manager,
            resources: ResourceStateManager::default(),
            paths,
        }
    }
}

impl TaskNotifier for DaemonNotifier {
    fn stage(&self, task_id: &str, _stage: &str, _progress: u8) {
        if let Ok(task) = self.manager.task(task_id) {
            self.resources.apply_task_update(task);
        }
        let _ = self.manager.persist(&self.paths);
    }

    fn log(&self, task_id: &str, line: &str) {
        if let Ok(task) = self.manager.task(task_id) {
            let entry = format!("[{}] {line}\n", box_foundation::now_seconds());
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(&task.log_path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, entry.as_bytes()));
        }
    }
}
