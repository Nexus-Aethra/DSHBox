//! Daemon state: the single process that owns the task queue, the bundled
//! Node/pnpm runtime, the running-container registry, and the resource
//! snapshot. CLI and desktop clients talk to this state via RPC; they never
//! construct their own copies.

use box_foundation::{read_config, BoxConfig, BoxPaths, BoxResult};
use box_runtime::{bundled::ResolvedBundledRuntime, process};
use box_scheduler::{TaskManager, TaskNotifier};
use box_state::ResourceStateManager;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, RwLock},
};

/// A managed host process (container server) owned by the daemon.
/// The daemon is the only owner since the desktop became a thin client;
/// clients observe hosts through RPC, never through this registry.
pub(crate) struct ManagedHost {
    pub(crate) handle: crate::runtime::HostHandle,
    pub(crate) url: String,
    /// The `dsh web:` URL parsed from the host log (per-launch capability
    /// token). `None` for DSH runtimes without the browser-trust fence, or
    /// before the host log announced it. `container_url` hands this to the
    /// webview; the bare `url` only proves the port is bound.
    pub(crate) authenticated_url: Option<String>,
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
    /// Runtime root (`<install>/runtime/<target>`). The `node/` and
    /// `pnpm/` trees hang directly off this directory on every platform,
    /// so consumers should derive sibling paths from here instead of
    /// counting `.parent()` levels off the node binary (the nesting
    /// depth differs between Windows `node/node.exe` and Unix
    /// `node/bin/node`).
    pub(crate) root: PathBuf,
    pub(crate) node: PathBuf,
    pub(crate) npm: PathBuf,
    pub(crate) pnpm: PathBuf,
    /// Directory that contains the bundled `git` entry point (e.g. the
    /// `cmd/` directory of PortableGit). `None` on targets where DSH Box
    /// has not yet published a bundled Git — the clean-room policy then
    /// leaves `PATH` and Git-related variables untouched.
    pub(crate) git_dir: Option<PathBuf>,
}

static BUNDLED_RUNTIME: OnceLock<BundledRuntime> = OnceLock::new();

pub(crate) fn bundled_target() -> &'static str {
    box_server_core::bundled_plugins::bundled_target()
}

/// Locate the resource directory (parent of `runtime/<target>/`) that
/// ships the bundled Node/pnpm. The deb/rpm bundle deploys the daemon at
/// `<install>/server/<target>/dshboxd` (tauri's resource map drops the
/// `resources/` level), so the resource root is three directories up from
/// the executable; developer builds fall back to `src-tauri/resources`
/// relative to this crate.
pub(crate) fn resource_directory() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("DSHBOX_RESOURCE_DIR") {
        return Ok(PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe() {
        // exe: resources/server/<target>/dshboxd
        if let Some(parent) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            if parent.join("runtime").is_dir() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    // Dev checkout: walk up to the directory holding `resources/runtime` rather than
    // assuming a depth. dshboxd lives in `crates/apps/` now, and a fixed `../../`
    // pointed at a directory that simply never existed.
    let mut dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dev.join("resources");
        if candidate.join("runtime").is_dir() {
            return Ok(candidate);
        }
        if !dev.pop() {
            break;
        }
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
        runtime.git_dir().as_deref(),
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
    let git_dir = runtime.git_dir();
    let root = runtime.root.clone();
    BUNDLED_RUNTIME
        .set(BundledRuntime {
            node_version: runtime.manifest.node_version,
            npm_version,
            pnpm_version: runtime.manifest.pnpm_version,
            root,
            node,
            npm,
            pnpm,
            git_dir,
        })
        .map_err(|_| "bundled runtime was initialized twice".to_owned())
}

pub(crate) fn bundled_runtime() -> Result<&'static BundledRuntime, String> {
    BUNDLED_RUNTIME
        .get()
        .ok_or("bundled runtime is unavailable; start dshboxd first".to_owned())
}

/// Vendor the bundled Cordis plugin tree (`resources/plugins/<target>/`,
/// carrying `@nexus-aethra/dsh-box-sandbox`) into `<runtimeDirectory>/plugins/`.
/// The implementation lives in box-server-core and is shared with the desktop
/// shell — the two private copies had already drifted (the desktop's deleted
/// the source tree in an installed layout); see its docs for the digest
/// idempotency and the installed-layout cleanup constraint.
pub(crate) fn initialize_bundled_plugins() -> Result<(), String> {
    match resource_directory() {
        Ok(root) => box_server_core::bundled_plugins::initialize_bundled_plugins(&root),
        Err(error) => {
            eprintln!("dshboxd: bundled plugins skipped: {error}");
            Ok(())
        }
    }
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
    /// Headless browser sessions backing the page-debugging RPCs, keyed by
    /// container id. A session holds a live DevTools socket, so it is kept
    /// between calls and torn down on close or daemon exit.
    pub(crate) browser: Mutex<crate::debug::Sessions>,
    /// Node registry (roadmap stage A). Shared with connection threads and
    /// the background sweep; clones share the same in-memory sessions.
    pub(crate) nodes: std::sync::Arc<crate::nodes::NodeManager>,
    /// Which prepared bases exist on which node (stage B1). An index, not the
    /// bytes: see the module doc for what is and is not verified here.
    pub(crate) artifacts: crate::node_artifacts::ArtifactIndex,
}

impl DaemonState {
    pub(crate) fn load() -> Result<Self, String> {
        let config = read_config().map_err(|error| format!("cannot read config: {error}"))?;
        let paths = BoxPaths::from_config(&config)
            .map_err(|error| format!("cannot derive box paths: {error}"))?;
        // SQLite document store with a one-time import of the legacy JSON
        // queue. If no runtime directory is configured yet (fresh install),
        // or the store cannot be opened (torn legacy file), degrade: JSON
        // backend when the legacy layout is usable, in-memory otherwise —
        // matching the old behaviour where a missing runtime only made
        // persistence impossible. The error is logged for diagnosis.
        let manager = match paths.runtime.as_ref() {
            None => {
                tracing::warn!("no runtime directory configured; task queue is memory-only");
                TaskManager::memory()
            }
            Some(_) => match box_store::open_task_collection(&paths) {
                Ok(collection) => TaskManager::new(collection),
                Err(error) => {
                    tracing::error!(
                        "SQLite task store unavailable, using legacy JSON store: {error}"
                    );
                    TaskManager::json(&paths).unwrap_or_else(|json_error| {
                        tracing::error!("JSON task store also unavailable: {json_error}");
                        TaskManager::memory()
                    })
                }
            },
        };
        // The daemon owns every worker it starts, so it sweeps everything. A
        // client (the desktop) shares this store and must sweep only its own
        // kinds -- see Sweep.
        let _ = manager.restore(&paths, box_scheduler::Sweep::Everything);
        let events = std::sync::Arc::new(crate::events::DaemonEvents::new());
        // Replaced by main.rs once the ephemeral listener is bound.
        let control_url = String::new();
        let nodes = std::sync::Arc::new(crate::nodes::NodeManager::load(
            &paths,
            control_url,
            events.clone(),
        ));
        let artifacts = crate::node_artifacts::ArtifactIndex::load(&paths);
        Ok(Self {
            manager,
            paths: RwLock::new(paths),
            containers: Arc::new(ContainerManager::default()),
            resources: ResourceStateManager::default(),
            events,
            browser: Mutex::new(crate::debug::Sessions::new()),
            nodes,
            artifacts,
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

/// Task notifier for daemon-run tasks: refreshes the resource snapshot and
/// broadcasts events to all `/events` subscribers.
///
/// It deliberately does **not** persist: the scheduler mutation behind every
/// notification already wrote that task's row under the state lock, so a
/// whole-scope `persist()` here would rewrite every other task from this
/// process's in-memory copy and undo a claim another writer just made (design
/// section 3.3, review finding B7). `stage` runs on every stage event, which
/// made it the most frequent way to lose one.
pub(crate) struct DaemonNotifier {
    manager: TaskManager,
    resources: ResourceStateManager,
    events: std::sync::Arc<crate::events::DaemonEvents>,
}

impl DaemonNotifier {
    pub(crate) fn from_paths(
        manager: TaskManager,
        events: std::sync::Arc<crate::events::DaemonEvents>,
    ) -> Self {
        Self {
            manager,
            resources: ResourceStateManager::default(),
            events,
        }
    }
}

impl TaskNotifier for DaemonNotifier {
    fn stage(&self, task_id: &str, stage: &str, progress: u8) {
        if let Ok(task) = self.manager.task(task_id) {
            self.resources.apply_task_update(task);
        }
        // No `persist()` here: see the note on this type.
        self.events
            .broadcast(crate::events::DaemonEvent::TaskStage {
                task_id: task_id.to_owned(),
                stage: stage.to_owned(),
                progress,
            });
        // The per-stage picture travels in its own event, so a client does not
        // have to poll to learn that one stage advanced while another finished.
        if let Ok(task) = self.manager.task(task_id) {
            if !task.stages.is_empty() {
                self.events
                    .broadcast(crate::events::DaemonEvent::TaskStages {
                        task_id: task_id.to_owned(),
                        stages: serde_json::to_value(&task.stages)
                            .unwrap_or(serde_json::Value::Null),
                    });
            }
        }
    }

    fn log(&self, task_id: &str, line: &str) {
        // The task context owns the log file; writing it here as well put every
        // line in it twice.
        self.events.broadcast(crate::events::DaemonEvent::TaskLog {
            task_id: task_id.to_owned(),
            line: line.to_owned(),
        });
    }

    fn finished(&self, task_id: &str, status: &str, error: Option<&str>) {
        if let Ok(task) = self.manager.task(task_id) {
            self.resources.apply_task_update(task);
        }
        // No `persist()` here: see the note on this type.
        self.events
            .broadcast(crate::events::DaemonEvent::TaskFinished {
                task_id: task_id.to_owned(),
                status: status.to_owned(),
                error: error.map(str::to_owned),
            });
    }
}

#[cfg(test)]
mod notifier_tests {
    use super::{DaemonNotifier, TaskNotifier};
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use std::{env, fs, sync::Arc};

    fn scratch(name: &str) -> BoxPaths {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_nanos())
            .unwrap_or(0);
        let root = env::temp_dir().join(format!("dshboxd-notifier-{name}-{stamp}"));
        let _ = fs::create_dir_all(&root);
        BoxPaths {
            config: root.join("config.json"),
            runtime: Some(root),
        }
    }

    /// A notification must not write the task table.
    ///
    /// This notifier fires on every stage tick, and its process shares the task
    /// store with whoever else writes tasks -- the desktop, and from stage B a
    /// node claim. A whole-scope `persist()` here snapshots this process's
    /// memory, so a row another writer just changed is rewritten from the stale
    /// copy and the task reads after a restart as running with no owner (design
    /// section 3.3, review finding B7).
    #[test]
    fn a_notification_does_not_rewrite_another_writers_row() {
        let paths = scratch("stage");
        let manager = TaskManager::json(&paths).unwrap();
        let a = manager
            .enqueue(&paths, "a", vec![], serde_json::json!({}))
            .unwrap();
        let b = manager
            .enqueue(&paths, "b", vec![], serde_json::json!({}))
            .unwrap();

        // Another writer claims B. This manager's copy of B is still `queued`,
        // and nothing below touches B.
        let other = TaskManager::json(&paths).unwrap();
        other.merge_from_disk(&paths).unwrap();
        other.update(&paths, &b.id, "claimed by node", 30).unwrap();

        let notifier = DaemonNotifier::from_paths(
            manager.clone(),
            Arc::new(crate::events::DaemonEvents::new()),
        );
        notifier.stage(&a.id, "running", 50);
        notifier.finished(&a.id, "succeeded", None);

        // Read the store back through a fresh manager: this manager's memory
        // would agree with itself whatever the notifier did.
        let reader = TaskManager::json(&paths).unwrap();
        reader.merge_from_disk(&paths).unwrap();
        let rows = reader.list().unwrap();
        let row = |id: &str| rows.iter().find(|task| task.id == id).unwrap().clone();
        assert_eq!(
            row(&b.id).stage,
            "claimed by node",
            "B7: the notification restored a stale copy of another writer's row"
        );
        assert_eq!(
            row(&a.id).status,
            "queued",
            "a notification persists nothing"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }
}
