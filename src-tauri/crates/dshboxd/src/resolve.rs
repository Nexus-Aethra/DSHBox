//! Daemon adapters over the container-reference resolver in box-containers.
//!
//! The matching rules, the join gate and the tests for them live in
//! `box_containers::resolve`; this module is what the RPC verbs call, because
//! they hold a `&DaemonState` whose paths carry the runtime root the pure
//! resolver needs. One rule of thumb: a new verb resolves here, at the
//! request boundary, and hands the canonical id downward.

use std::path::{Path, PathBuf};

use box_containers::resolve::ResolvedContainer;

use crate::state::DaemonState;

/// The runtime root, in the wording every RPC already uses when storage is unset.
pub(crate) fn runtime_root(state: &DaemonState) -> Result<PathBuf, String> {
    state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?
        .runtime
        .clone()
        .ok_or_else(|| "DSH Box storage is not configured".to_owned())
}

/// Root-based form for callers that hold a `BoxPaths` clone (task closures)
/// rather than `&DaemonState`: same rules, same errors as the state-based
/// wrapper below.
pub(crate) fn resolve_container_in_root(
    root: &Path,
    raw: &str,
) -> Result<ResolvedContainer, String> {
    box_containers::resolve::resolve_container_in_root(root, raw)
}

/// Resolve how a caller refers to a container into one canonical record.
pub(crate) fn resolve_container_id(
    state: &DaemonState,
    raw: &str,
) -> Result<ResolvedContainer, String> {
    let root = runtime_root(state)?;
    resolve_container_in_root(&root, raw)
}

/// Resolve a reference to its on-disk directory.
pub(crate) fn resolve_container_dir(state: &DaemonState, raw: &str) -> Result<PathBuf, String> {
    resolve_container_id(state, raw).map(|found| found.directory)
}

/// Resolve a reference and keep only the canonical id, for verbs that key an
/// in-memory registry rather than reading a directory.
pub(crate) fn resolve_container_key(state: &DaemonState, raw: &str) -> Result<String, String> {
    resolve_container_id(state, raw).map(|found| found.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ContainerManager;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use box_state::ResourceStateManager;
    use std::path::Path;
    use std::sync::{Arc, Mutex, RwLock};

    /// A daemon state pointed at a scratch runtime root. Built directly rather
    /// than through `DaemonState::load`: that reads `~/.dsh-box/config.json`,
    /// and a test that mutates HOME has to hold the process-global env lock for
    /// it. Resolution only ever needs `paths.runtime`.
    fn state_at(root: &Path) -> DaemonState {
        state_at_with(root, Some(root.to_path_buf()))
    }

    fn state_at_with(root: &Path, runtime: Option<std::path::PathBuf>) -> DaemonState {
        DaemonState {
            manager: TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: root.join("config.json"),
                runtime,
            }),
            containers: Arc::new(ContainerManager::default()),
            resources: ResourceStateManager::default(),
            events: Arc::new(crate::events::DaemonEvents::new()),
            browser: Mutex::new(crate::debug::Sessions::new()),
        }
    }

    #[test]
    fn an_unconfigured_store_is_reported_the_way_every_rpc_reports_it() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-resolve-no-runtime-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state = state_at_with(&root, None);
        let error = resolve_container_id(&state, "alpha").unwrap_err();
        assert_eq!(error, "DSH Box storage is not configured");
    }
}