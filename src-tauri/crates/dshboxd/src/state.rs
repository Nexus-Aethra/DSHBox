//! Daemon state: the single process that owns the task queue, the bundled
//! Node/pnpm runtime, the running-container registry, and the resource
//! snapshot. CLI and desktop clients talk to this state via RPC; they never
//! construct their own copies.

use box_foundation::{read_config, BoxConfig, BoxPaths, BoxResult};
use box_runtime::{bundled::ResolvedBundledRuntime, process::{self, TrackedChild}};
use box_scheduler::{TaskManager, TaskNotifier};
use box_state::ResourceStateManager;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

/// A managed host process (container server) owned by the daemon.
/// Mirrors the desktop's `ManagedHost`; clients never hold these.
pub(crate) struct ManagedHost {
    pub(crate) child: TrackedChild,
    pub(crate) url: String,
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
pub(crate) fn resource_directory() -> Result<PathBuf, String> {
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

/// Resolve the dshbox installation root directory (e.g. `D:\dshbox\`).
///
/// The resource directory is `<install>/resources/`, so the install root
/// is its parent. Falls back to the resource directory itself when the
/// layout is unconventional (developer builds, custom DSHBOX_RESOURCE_DIR).
pub(crate) fn dshbox_install_directory() -> Result<PathBuf, String> {
    let resource = resource_directory()?;
    // For a deployed install: resources/ → install root is parent.
    // For developer builds or custom env-var overrides, stay at the resource
    // dir — the install-root concept doesn't apply in those cases.
    if resource.ends_with("resources") || resource.ends_with("resources\\") {
        if let Some(parent) = resource.parent() {
            return Ok(parent.to_path_buf());
        }
    }
    Ok(resource)
}

/// Initialize the bundled runtime exactly once. Called during daemon
/// startup so every task worker can resolve node/npm/pnpm.
pub(crate) fn initialize_bundled_runtime() -> Result<(), String> {
    let root = resource_directory()?.join("runtime").join(bundled_target());
    let runtime = ResolvedBundledRuntime::from_path(&root).map_err(|error| {
        format!(
            "bundled runtime is missing for {}: {error}",
            bundled_target()
        )
    })?;
    let node = runtime.node_executable();
    let npm = runtime.npm_script();
    let pnpm = runtime.pnpm_script();
    if !node.is_file() || !npm.is_file() || !pnpm.is_file() {
        return Err("bundled runtime is incomplete; reinstall DSH Box".to_owned());
    }
    let policy = process::bundled_toolchain_policy(
        dshbox_install_directory().ok().as_deref(),
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        None,
        None,
        false,
    );
    let spec = process::ProcessSpec::new(&node)
        .arg(&npm)
        .arg("--version")
        .policy(policy);
    let npm_version = process::NativeProcessRunner
        .run(&spec)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    BUNDLED_RUNTIME
        .set(BundledRuntime {
            node_version: runtime.manifest.node_version,
            npm_version,
            pnpm_version: runtime.manifest.pnpm_version,
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
    let Some(runtime_directory) = config.runtime_directory.as_ref() else {
        return Ok(());
    };
    let runtime_root = PathBuf::from(runtime_directory);
    if !runtime_root.is_dir() {
        return Ok(());
    }

    let cache_root = runtime_root.join("plugins");
    // The digest only proves "this manifest was vendored once"; it travels
    // inside the config and therefore survives a storage-root switch
    // (`config set runtime` / onboarding). Short-circuit only when the
    // vendored tree itself exists in the CURRENT runtime directory —
    // otherwise every container start dies with ERR_MODULE_NOT_FOUND for
    // @deepseek-ai/dsh-box-context while the daemon believes it is done.
    let digest_matches = config.plugins_manifest_digest.as_deref() == Some(digest.as_str());
    if digest_matches && cache_root.join("plugins-manifest.json").is_file() {
        return Ok(());
    }
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
    /// Box paths derived from the persisted config.
    pub(crate) paths: RwLock<BoxPaths>,
    /// Container registry shared with task workers.
    pub(crate) containers: Arc<ContainerManager>,
    /// Reserved for upcoming resource RPCs.
    #[allow(dead_code)]
    pub(crate) resources: ResourceStateManager,
    /// Event bus for SSE streaming. Every task progress update, log line,
    /// and resource change is broadcast to all `/events` subscribers.
    pub(crate) events: std::sync::Arc<crate::events::DaemonEvents>,
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
            events: std::sync::Arc::new(crate::events::DaemonEvents::new()),
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

/// Task notifier for daemon-run tasks: persists progress, refreshes the
/// resource snapshot, and broadcasts events to all `/events` subscribers.
pub(crate) struct DaemonNotifier {
    manager: TaskManager,
    resources: ResourceStateManager,
    paths: BoxPaths,
    events: std::sync::Arc<crate::events::DaemonEvents>,
}

impl DaemonNotifier {
    pub(crate) fn from_paths(
        manager: TaskManager,
        paths: BoxPaths,
        events: std::sync::Arc<crate::events::DaemonEvents>,
    ) -> Self {
        Self {
            manager,
            resources: ResourceStateManager::default(),
            paths,
            events,
        }
    }
}

impl TaskNotifier for DaemonNotifier {
    fn stage(&self, task_id: &str, stage: &str, progress: u8) {
        if let Ok(task) = self.manager.task(task_id) {
            self.resources.apply_task_update(task);
        }
        let _ = self.manager.persist(&self.paths);
        self.events.broadcast(crate::events::DaemonEvent::TaskStage {
            task_id: task_id.to_owned(),
            stage: stage.to_owned(),
            progress,
        });
    }

    fn log(&self, task_id: &str, line: &str) {
        if let Ok(task) = self.manager.task(task_id) {
            let entry = format!("[{}] {line}\n", box_foundation::now_seconds());
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(&task.log_path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, entry.as_bytes()));
        }
        self.events.broadcast(crate::events::DaemonEvent::TaskLog {
            task_id: task_id.to_owned(),
            line: line.to_owned(),
        });
    }

    fn finished(
        &self,
        task_id: &str,
        status: &str,
        error: Option<&str>,
    ) {
        if let Ok(task) = self.manager.task(task_id) {
            self.resources.apply_task_update(task);
        }
        let _ = self.manager.persist(&self.paths);
        self.events.broadcast(crate::events::DaemonEvent::TaskFinished {
            task_id: task_id.to_owned(),
            status: status.to_owned(),
            error: error.map(str::to_owned),
        });
    }
}
