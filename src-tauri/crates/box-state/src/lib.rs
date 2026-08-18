//! Process-local resource state and a diagnostic JSON snapshot.
//!
//! Filesystem scanners remain the source of truth. This manager only provides a
//! consistent, read-only view while the desktop application is running.

use box_containers::DshContainer;
use box_dsh_versions::DshVersion;
use box_extensions::{read_bundles, scan_container_extensions, scan_repository, ContainerExtensions, ExtensionBundle, RepositoryExtension};
use box_foundation::{now_seconds, BoxConfig, BoxPaths, BoxResult};
use box_scheduler::TaskRecord;
use box_toolchains::ToolchainStatus;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResourceKind {
    // System-level kinds
    Toolchain,
    Runtime,
    Container,
    Task,
    // User-facing resource kinds. The Resources page surfaces the official
    // harness as a "Harness" tab, but the backing resource is the same as
    // every other template — there is no separate `Harness` kind any more.
    Template,
    Plugin,
    Bundle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResourceHealth {
    Ready,
    Missing,
    Busy,
    Failed,
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceState {
    pub resource_key: String,
    pub kind: ResourceKind,
    pub name: String,
    pub health: ResourceHealth,
    pub detail: Option<String>,
    pub progress: Option<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub runtime_directory: Option<String>,
    pub language: String,
    pub selected_dsh_version: Option<String>,
    pub toolchains: Vec<ToolchainStatus>,
    pub versions: Vec<DshVersion>,
    pub containers: Vec<DshContainer>,
    #[serde(default)]
    pub container_extensions: BTreeMap<String, ContainerExtensions>,
    #[serde(default)]
    pub extension_repository: Vec<RepositoryExtension>,
    /// Reference counts for repository extensions: how many containers and
    /// built templates currently link each entry. Drives the UI's usage
    /// badge and protects entries from `plugin prune`. Empty values
    /// (containers+templates=0) are pruned on read.
    #[serde(default)]
    pub repository_references: BTreeMap<String, RepositoryReferenceCount>,
    pub tasks: Vec<TaskRecord>,
    pub resources: BTreeMap<String, ResourceState>,
    pub scanned_at: u64,
    pub updated_at: u64,
}

/// Mirror of `box_extensions::ReferenceCount` for the UI snapshot. Kept
/// in this crate (instead of imported from `box-extensions`) so the UI can
/// be compiled independently from the backend storage types.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryReferenceCount {
    #[serde(default)]
    pub containers: u32,
    #[serde(default)]
    pub templates: u32,
}

impl RepositoryReferenceCount {
    pub fn total(&self) -> u32 {
        self.containers + self.templates
    }
}

impl Default for ResourceSnapshot {
    fn default() -> Self {
        let now = now_seconds();
        Self {
            runtime_directory: None,
            language: "en".to_owned(),
            selected_dsh_version: None,
            toolchains: Vec::new(),
            versions: Vec::new(),
            containers: Vec::new(),
            container_extensions: BTreeMap::new(),
            extension_repository: Vec::new(),
            repository_references: BTreeMap::new(),
            tasks: Vec::new(),
            resources: BTreeMap::new(),
            scanned_at: now,
            updated_at: now,
        }
    }
}

/// The single in-memory query surface for every managed resource.
#[derive(Clone, Default)]
pub struct ResourceStateManager {
    snapshot: Arc<RwLock<ResourceSnapshot>>,
}

impl ResourceStateManager {
    /// Creates state from a fresh scan; persisted snapshots are deliberately ignored.
    pub fn bootstrap(
        config: &BoxConfig,
        toolchains: Vec<ToolchainStatus>,
        versions: Vec<DshVersion>,
        containers: Vec<DshContainer>,
        tasks: Vec<TaskRecord>,
    ) -> Self {
        let manager = Self::default();
        manager.refresh_all(config, toolchains, versions, containers);
        manager.replace_tasks(tasks);
        manager
    }

    pub fn snapshot(&self) -> BoxResult<ResourceSnapshot> {
        self.snapshot
            .read()
            .map_err(|_| "resource state lock failed".to_owned())
            .map(|state| state.clone())
    }

    pub fn resource(&self, key: &str) -> BoxResult<Option<ResourceState>> {
        Ok(self.snapshot()?.resources.get(key).cloned())
    }

    pub fn replace_tasks(&self, tasks: Vec<TaskRecord>) {
        self.mutate(|state| state.tasks = tasks);
    }

    /// Applies an emitted scheduler record without requiring a complete rescan.
    pub fn apply_task_update(&self, task: TaskRecord) {
        self.mutate(|state| {
            if let Some(existing) = state
                .tasks
                .iter_mut()
                .find(|existing| existing.id == task.id)
            {
                *existing = task;
            } else {
                state.tasks.push(task);
            }
        });
    }

    pub fn refresh_toolchain(&self, toolchain: ToolchainStatus) {
        self.mutate(|state| {
            if let Some(existing) = state
                .toolchains
                .iter_mut()
                .find(|item| item.id == toolchain.id)
            {
                *existing = toolchain;
            } else {
                state.toolchains.push(toolchain);
            }
        });
    }

    pub fn refresh_runtime(&self, version: DshVersion) {
        self.mutate(|state| {
            if let Some(existing) = state
                .versions
                .iter_mut()
                .find(|item| item.name == version.name)
            {
                *existing = version;
            } else {
                state.versions.push(version);
            }
        });
    }

    pub fn refresh_container(&self, container: DshContainer) {
        self.mutate(|state| {
            let extensions = scan_container_extensions(&container);
            if let Some(existing) = state
                .containers
                .iter_mut()
                .find(|item| item.id == container.id)
            {
                *existing = container;
            } else {
                state.containers.push(container);
            }
            state
                .container_extensions
                .insert(extensions.container_id.clone(), extensions);
        });
    }

    /// Returns the cached read-only contents of one managed container.
    pub fn container_extensions(&self, id: &str) -> BoxResult<Option<ContainerExtensions>> {
        Ok(self.snapshot()?.container_extensions.get(id).cloned())
    }

    pub fn refresh_all(
        &self,
        config: &BoxConfig,
        toolchains: Vec<ToolchainStatus>,
        versions: Vec<DshVersion>,
        containers: Vec<DshContainer>,
    ) {
        self.mutate(|state| {
            state.runtime_directory = config.runtime_directory.clone();
            state.language = config.language.clone();
            state.selected_dsh_version = config.selected_dsh_version.clone();
            state.toolchains = toolchains;
            state.versions = versions;
            state.container_extensions = containers
                .iter()
                .map(|container| (container.id.clone(), scan_container_extensions(container)))
                .collect();
            state.extension_repository = config.runtime_directory.as_deref().map(|runtime| scan_repository(std::path::Path::new(runtime))).unwrap_or_default();
            state.repository_references = config
                .runtime_directory
                .as_deref()
                .map(|runtime| {
                    box_extensions::read_references(std::path::Path::new(runtime))
                        .into_iter()
                        .map(|(id, count)| {
                            // Snapshot shape: numbers only. The disk
                            // owner sets are projected to their length
                            // for the UI's "in use by" badge; the
                            // detailed ids live behind the
                            // `list_repository_reference_counts` RPC.
                            (
                                id,
                                RepositoryReferenceCount {
                                    containers: count.containers.len() as u32,
                                    templates: count.templates.len() as u32,
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            state.containers = containers;
            state.scanned_at = now_seconds();
        });
    }

    pub fn write_snapshot(&self, paths: &BoxPaths) -> BoxResult<()> {
        let runtime = paths
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?;
        let path = runtime.join("state/resources.json");
        fs::create_dir_all(path.parent().ok_or("resource state path has no parent")?)
            .map_err(|error| error.to_string())?;
        fs::write(
            path,
            serde_json::to_string_pretty(&self.snapshot()?).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }

    fn mutate(&self, change: impl FnOnce(&mut ResourceSnapshot)) {
        if let Ok(mut state) = self.snapshot.write() {
            change(&mut state);
            rebuild_resources(&mut state);
            state.updated_at = now_seconds();
        }
    }
}

fn rebuild_resources(state: &mut ResourceSnapshot) {
    let mut resources = BTreeMap::new();
    for toolchain in &state.toolchains {
        let source = state.toolchains.iter().find(|item| item.id == toolchain.id);
        let version = source.and_then(|item| {
            item.managed_version
                .as_ref()
                .or(item.system_version.as_ref())
        });
        resources.insert(
            format!("toolchain:{}", toolchain.id),
            ResourceState {
                resource_key: format!("toolchain:{}", toolchain.id),
                kind: ResourceKind::Toolchain,
                name: toolchain.name.clone(),
                health: if version.is_some() {
                    ResourceHealth::Ready
                } else {
                    ResourceHealth::Missing
                },
                detail: version.cloned(),
                progress: None,
            },
        );
    }
    for version in &state.versions {
        if version.installed {
            resources.insert(
                format!("runtime:{}", version.name),
                ResourceState {
                    resource_key: format!("runtime:{}", version.name),
                    kind: ResourceKind::Runtime,
                    name: version.name.clone(),
                    health: ResourceHealth::Ready,
                    detail: None,
                    progress: None,
                },
            );
        }
    }
    for container in &state.containers {
        resources.insert(
            format!("container:{}", container.id),
            ResourceState {
                resource_key: format!("container:{}", container.id),
                kind: ResourceKind::Container,
                name: container.name.clone(),
                health: if container.status == "running" {
                    ResourceHealth::Ready
                } else {
                    ResourceHealth::Unknown
                },
                detail: Some(container.version.clone()),
                progress: None,
            },
        );
    }
    for task in &state.tasks {
        let health = match task.status.as_str() {
            "queued" | "running" | "waiting_input" => ResourceHealth::Busy,
            "failed" | "interrupted" => ResourceHealth::Failed,
            _ => ResourceHealth::Ready,
        };
        for key in &task.resource_keys {
            if let Some(resource) = resources.get_mut(key) {
                if health == ResourceHealth::Busy || health == ResourceHealth::Failed {
                    resource.health = health.clone();
                    resource.progress = Some(task.progress);
                    if health == ResourceHealth::Failed {
                        resource.detail = task.error.clone().or_else(|| Some(task.stage.clone()));
                    }
                }
            }
        }
        resources.insert(
            format!("task:{}", task.id),
            ResourceState {
                resource_key: format!("task:{}", task.id),
                kind: ResourceKind::Task,
                name: task.kind.clone(),
                health,
                detail: task.error.clone().or_else(|| Some(task.stage.clone())),
                progress: Some(task.progress),
            },
        );
    }
    // Plugin: repository extensions of kind Plugin
    for entry in &state.extension_repository {
        if entry.kind == box_extensions::ExtensionKind::Plugin {
            resources.insert(
                format!("plugin:{}", entry.id),
                ResourceState {
                    resource_key: format!("plugin:{}", entry.id),
                    kind: ResourceKind::Plugin,
                    name: entry.name.clone(),
                    health: if entry.diagnostic.is_some() {
                        ResourceHealth::Failed
                    } else {
                        ResourceHealth::Ready
                    },
                    detail: entry.version.clone(),
                    progress: None,
                },
            );
        }
    }
    // Bundle: extension bundles
    if let Some(runtime_dir) = &state.runtime_directory {
        let bundles: Vec<ExtensionBundle> = read_bundles(Path::new(runtime_dir));
        for bundle in &bundles {
            resources.insert(
                format!("bundle:{}", bundle.id),
                ResourceState {
                    resource_key: format!("bundle:{}", bundle.id),
                    kind: ResourceKind::Bundle,
                    name: bundle.name.clone(),
                    health: ResourceHealth::Ready,
                    detail: Some(format!("{} entries", bundle.entries.len())),
                    progress: None,
                },
            );
        }
    }
    state.resources = resources;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn task(status: &str) -> TaskRecord {
        TaskRecord {
            id: "1".to_owned(),
            kind: "install".to_owned(),
            resource_keys: vec!["toolchain:node".to_owned()],
            status: status.to_owned(),
            stage: status.to_owned(),
            progress: 42,
            created_at: 1,
            started_at: None,
            finished_at: None,
            log_path: String::new(),
            error: Some("broken".to_owned()),
            params: serde_json::json!({}),
            cancel_requested: false,
        }
    }
    fn toolchain() -> ToolchainStatus {
        ToolchainStatus {
            id: "node".to_owned(),
            name: "Node.js".to_owned(),
            system_version: Some("v1".to_owned()),
            managed_version: None,
        }
    }

    #[test]
    fn task_lock_marks_its_resource_busy() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![task("running")],
        );
        assert_eq!(
            manager.resource("toolchain:node").unwrap().unwrap().health,
            ResourceHealth::Busy
        );
    }

    #[test]
    fn failed_task_is_reported_on_resource() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![task("failed")],
        );
        assert_eq!(
            manager.resource("toolchain:node").unwrap().unwrap().health,
            ResourceHealth::Failed
        );
    }

    #[test]
    fn bootstrap_uses_fresh_facts() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![],
        );
        manager.refresh_all(&BoxConfig::default(), vec![], vec![], vec![]);
        assert!(manager.snapshot().unwrap().resources.is_empty());
        let _ = BTreeMap::<String, String>::new();
    }
}
