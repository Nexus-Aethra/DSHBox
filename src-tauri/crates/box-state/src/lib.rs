//! Process-local resource state and a diagnostic JSON snapshot.
//!
//! Filesystem scanners remain the source of truth. This manager only provides a
//! consistent, read-only view while the desktop application is running.

use box_containers::DshContainer;
use box_dsh_versions::DshVersion;
use box_extensions::{scan_container_extensions, scan_repository, ContainerExtensions, RepositoryExtension};
use box_foundation::{now_seconds, BoxConfig, BoxPaths, BoxResult};
use box_scheduler::TaskRecord;
use box_toolchains::ToolchainStatus;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResourceKind {
    Toolchain,
    Runtime,
    Container,
    Task,
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
    pub tasks: Vec<TaskRecord>,
    pub resources: BTreeMap<String, ResourceState>,
    pub scanned_at: u64,
    pub updated_at: u64,
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
