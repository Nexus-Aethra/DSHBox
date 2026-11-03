//! One container identifier, resolved the same way everywhere.
//!
//! A container's id is `container-<timestamp>`, which nobody types from
//! memory. An agent told to point a tool at a container therefore reaches for
//! what it can see: the name it created it with, or the bare timestamp part of
//! the id. Every daemon verb that joined that string straight onto `instances/`
//! answered `container not found`, and the error gave nothing back to try, so a
//! real agent spent its time guessing the format before doing the one thing it
//! was asked (docs/design/2026-11-03-agent-experience-tooling.md).
//!
//! `apply` had already solved this: its resolver accepted an id or a name and
//! listed the known containers on failure. But it solved it only for `apply`,
//! and a dozen other entry points each kept their own exact match. So the fix
//! here is not a thirteenth resolver: resolve once at the request boundary and
//! hand the canonical id downward, which also means the in-memory registries
//! (running hosts, browser sessions) are never keyed by an alias.
//!
//! Safety: this is the only place that touches the filesystem with a caller's
//! raw string, so the shape check runs before any join. `is_safe_identifier`
//! permits dots, and `instances/..` is a real directory: joined blindly it
//! returns the runtime root, which is every container on the machine. Dots are
//! legitimate inside an id, so a segment that is empty or all dots is refused
//! rather than sanitized.

use std::path::{Path, PathBuf};

use box_containers::DshContainer;
use serde_json::Value;

use crate::state::DaemonState;

/// A resolved container: canonical id and its on-disk directory.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedContainer {
    pub(crate) id: String,
    pub(crate) directory: PathBuf,
}

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

/// Reject anything that cannot name a container, before it reaches a path join.
fn check_shape(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("expected a container id or name".to_owned());
    }
    Ok(())
}

/// The gate for the one spelling that joins a path: `instances/<raw>`. Only a
/// safe identifier with no empty dot-segment may be joined — this covers ".",
/// "..", "..." and separators, each of which is a directory that exists next
/// to `instances/<id>`. A reference this gate rejects still goes to name
/// matching: names are only length-checked at creation, so a legal name can
/// carry spaces, dots or any Unicode, and name matching returns directories
/// taken from scanned records, never a join of the raw string.
fn check_joinable(raw: &str) -> Result<(), String> {
    if !box_foundation::is_safe_identifier(raw) {
        return Err(format!("invalid container id: {raw:?}"));
    }
    if raw.split('.').any(|segment| segment.is_empty()) {
        return Err(format!("invalid container id: {raw:?}"));
    }
    Ok(())
}

/// Every container under one runtime root, through the scanner rather than by
/// listing `instances/`: a directory with no readable `container.json` (an
/// aborted create) is not a container a reference should resolve to.
fn containers_in(root: &Path) -> Result<Vec<DshContainer>, String> {
    let scanned = box_containers::scan_containers(&root.to_string_lossy())
        .map_err(|error| format!("cannot scan containers: {error}"))?;
    Ok(scanned.values().cloned().collect())
}

/// Resolve how a caller refers to a container into one canonical record.
///
/// Accepts three spellings: the full id (`container-1730...`), the name the
/// user created the container with, and the id without its `container-`
/// prefix. A name held by two containers is an error rather than a first
/// match: picking silently would send a write to whichever one the directory
/// scan happened to visit first.
pub(crate) fn resolve_container_id(
    state: &DaemonState,
    raw: &str,
) -> Result<ResolvedContainer, String> {
    check_shape(raw)?;
    let root = runtime_root(state)?;
    match fast_lookup(&root, raw)? {
        Some(found) => Ok(found),
        None => match_container(&containers_in(&root)?, raw),
    }
}

/// Resolve inside a runtime root the caller already holds.
///
/// A task closure gets a `BoxPaths` clone rather than `&DaemonState`, and it
/// still has to resolve the way a request does: two matching rules for one
/// identifier is how `container not found` comes back for a container that
/// exists, which is the exact confusion this module replaces.
pub(crate) fn resolve_container_in_root(
    root: &Path,
    raw: &str,
) -> Result<ResolvedContainer, String> {
    check_shape(raw)?;
    match fast_lookup(root, raw)? {
        Some(found) => Ok(found),
        None => match_container(&containers_in(root)?, raw),
    }
}

/// The exact-id case, answered without scanning the store.
///
/// `None` means "not decided yet", not "missing": anything the directory alone
/// cannot settle is handed to the scan, which is what refuses a half-created
/// container with no readable record. The scan is not free -- `scan_containers`
/// probes each container's host PID so `dshbox ps` reports live state -- and
/// the page tools resolve a reference on every call, so paying for it on the
/// one spelling that needs no lookup would tax every read to fund a name fix.
fn fast_lookup(root: &Path, raw: &str) -> Result<Option<ResolvedContainer>, String> {
    // The raw string is about to join a path, so only a joinable reference
    // may take this shortcut; anything else defers to the scan, whose
    // records never join the raw string.
    if check_joinable(raw).is_err() {
        return Ok(None);
    }
    let directory = root.join("instances").join(raw);
    if !directory.is_dir() {
        return Ok(None);
    }
    let Ok(text) = std::fs::read_to_string(directory.join("container.json")) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    if value["id"].as_str() != Some(raw) {
        // A directory named for one container whose record names another.
        // Believing the directory over the record would hand the caller a
        // container it never asked for, so this defers to the scan.
        return Ok(None);
    }
    Ok(Some(ResolvedContainer {
        id: raw.to_owned(),
        directory,
    }))
}

/// One match is a match; several are an ambiguity to report, never a guess.
fn one_of<'c>(
    candidates: Vec<&'c DshContainer>,
    raw: &str,
    what: &str,
) -> Result<Option<&'c DshContainer>, String> {
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(candidates[0])),
        _ => {
            let ids = candidates
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!("ambiguous container {what} {raw:?}; ids are: {ids}"))
        }
    }
}

/// The accepted spellings, then the honest failure.
///
/// Full id; bare timestamp (exact `container-<ts>` for timestamped ids, and
/// the `container-<ts>-` prefix for the sealed-template shape that carries a
/// task id after it); then the name, which may contain anything — see
/// `check_joinable` for why no charset applies here.
fn match_container(containers: &[DshContainer], raw: &str) -> Result<ResolvedContainer, String> {
    let mut matched: Option<&DshContainer> = containers.iter().find(|c| c.id == raw);
    if matched.is_none() {
        let prefixed = format!("container-{raw}");
        matched = one_of(
            containers.iter().filter(|c| c.id == prefixed).collect(),
            raw,
            "id prefix",
        )?;
    }
    if matched.is_none() {
        let prefix = format!("container-{raw}-");
        matched = one_of(
            containers.iter().filter(|c| c.id.starts_with(&prefix)).collect(),
            raw,
            "id prefix",
        )?;
    }
    if matched.is_none() {
        matched = one_of(
            containers.iter().filter(|c| c.name == raw).collect(),
            raw,
            "name",
        )?;
    }

    if let Some(found) = matched {
        return Ok(ResolvedContainer {
            id: found.id.clone(),
            directory: PathBuf::from(&found.directory),
        });
    }

    // An unresolvable reference has to say what would have worked.
    // "container not found: x" leaves the caller guessing whether the store,
    // the name, or the id format is wrong; naming the containers that do exist
    // ends the guess, which is the difference between an agent recovering and
    // an agent retrying the same typo four times.
    let mut labels: Vec<&str> = containers.iter().map(|c| c.name.as_str()).collect();
    labels.sort_unstable();
    if labels.is_empty() {
        Err(format!(
            "no container named {raw:?}, and there are no containers yet"
        ))
    } else {
        Err(format!(
            "no container named {raw:?}; the containers are: {}",
            labels.join(", ")
        ))
    }
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
    use std::fs;
    use std::sync::{Arc, Mutex, RwLock};

    /// A daemon state pointed at a scratch runtime root. Built directly rather
    /// than through `DaemonState::load`: that reads `~/.dsh-box/config.json`,
    /// and a test that mutates HOME has to hold the process-global env lock for
    /// it. Resolution only ever needs `paths.runtime`.
    fn state_at(root: &Path) -> DaemonState {
        DaemonState {
            manager: TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: root.join("config.json"),
                runtime: Some(root.to_path_buf()),
            }),
            containers: Arc::new(ContainerManager::default()),
            resources: ResourceStateManager::default(),
            events: Arc::new(crate::events::DaemonEvents::new()),
            browser: Mutex::new(crate::debug::Sessions::new()),
        }
    }

    /// A container as the scanner sees it: an `instances/<id>` directory whose
    /// `container.json` carries the identity.
    fn make_container(root: &Path, id: &str, name: &str) {
        let directory = root.join("instances").join(id);
        fs::create_dir_all(directory.join("state")).unwrap();
        fs::write(
            directory.join("container.json"),
            serde_json::json!({
                "id": id,
                "name": name,
                "version": "latest",
                "profile": "web",
            })
            .to_string(),
        )
        .unwrap();
    }

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-resolve-{label}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_full_id_resolves_as_it_always_did() {
        let root = scratch("full-id");
        make_container(&root, "container-1730000000", "alpha");
        let found = resolve_container_id(&state_at(&root), "container-1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000");
        assert!(found.directory.is_dir());
    }

    #[test]
    fn a_name_resolves_to_the_same_container() {
        let root = scratch("name");
        make_container(&root, "container-1730000001", "alpha");
        let found = resolve_container_id(&state_at(&root), "alpha").unwrap();
        assert_eq!(found.id, "container-1730000001");
        assert_eq!(
            resolve_container_key(&state_at(&root), "alpha").unwrap(),
            "container-1730000001"
        );
    }

    #[test]
    fn the_bare_timestamp_part_resolves() {
        let root = scratch("bare");
        make_container(&root, "container-1730000002", "alpha");
        let found = resolve_container_id(&state_at(&root), "1730000002").unwrap();
        assert_eq!(found.id, "container-1730000002");
    }

    #[test]
    fn an_unknown_reference_lists_the_containers_that_exist() {
        let root = scratch("unknown");
        make_container(&root, "container-1730000003", "alpha");
        make_container(&root, "container-1730000004", "beta");
        let error = resolve_container_id(&state_at(&root), "nope").unwrap_err();
        assert!(error.contains("alpha"), "{error}");
        assert!(error.contains("beta"), "{error}");
    }

    #[test]
    fn an_empty_store_says_so_instead_of_listing_nothing() {
        let root = scratch("empty");
        let error = resolve_container_id(&state_at(&root), "anything").unwrap_err();
        assert!(error.contains("no containers yet"), "{error}");
    }

    #[test]
    fn two_containers_sharing_a_name_are_ambiguous_not_first_match() {
        let root = scratch("dupe");
        make_container(&root, "container-1730000005", "same");
        make_container(&root, "container-1730000006", "same");
        let error = resolve_container_id(&state_at(&root), "same").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("container-1730000005"), "{error}");
    }

    #[test]
    fn a_directory_without_container_json_is_not_resolvable() {
        let root = scratch("broken");
        fs::create_dir_all(root.join("instances/container-1730000008")).unwrap();
        let error = resolve_container_id(&state_at(&root), "container-1730000008").unwrap_err();
        assert!(error.contains("no container named"), "{error}");
    }

    #[test]
    fn a_sealed_style_timestamp_prefix_resolves() {
        let root = scratch("sealed-prefix");
        make_container(&root, "container-1730000000-4f2a", "sealed one");
        make_container(&root, "container-1730000001-9b3c", "sealed two");
        let found = resolve_container_id(&state_at(&root), "1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000-4f2a");
        // Two ids sharing the timestamp prefix are an ambiguity, not a guess.
        make_container(&root, "container-1730000000-77ee", "sealed three");
        let error = resolve_container_id(&state_at(&root), "1730000000").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn a_parent_reference_never_reaches_the_filesystem() {
        let root = scratch("escape");
        make_container(&root, "container-1730000007", "alpha");
        // `instances/..` is the runtime root: joining it blindly would hand
        // back the directory holding every container, and a destructive verb
        // would delete the store. The join gate refuses these before any path
        // is built, and the scan never joins the raw string — so the fallback
        // is an honest miss, not a traversal.
        for attempt in ["..", ".", "...", "..a", "a."] {
            let error = resolve_container_id(&state_at(&root), attempt).unwrap_err();
            assert!(error.contains("no container named"), "{attempt}: {error}");
        }
    }

    #[test]
    fn a_blank_reference_is_rejected_and_a_spaced_name_resolves() {
        let root = scratch("shape");
        for attempt in ["", "   "] {
            let error = resolve_container_id(&state_at(&root), attempt).unwrap_err();
            assert!(error.contains("expected a container"), "{attempt:?}: {error}");
        }
        // Names are only length-checked at creation: a legal name can carry
        // spaces, slashes or any Unicode, and matching it must not be
        // narrowed by the charset the id path-join needs.
        make_container(&root, "container-1730000009", "dev box/开发");
        let found = resolve_container_id(&state_at(&root), "dev box/开发").unwrap();
        assert_eq!(found.id, "container-1730000009");
    }

    #[test]
    fn an_unconfigured_store_is_reported_the_way_every_rpc_reports_it() {
        let root = scratch("no-runtime");
        let state = DaemonState {
            manager: TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: root.join("config.json"),
                runtime: None,
            }),
            containers: Arc::new(ContainerManager::default()),
            resources: ResourceStateManager::default(),
            events: Arc::new(crate::events::DaemonEvents::new()),
            browser: Mutex::new(crate::debug::Sessions::new()),
        };
        let error = resolve_container_id(&state, "alpha").unwrap_err();
        assert_eq!(error, "DSH Box storage is not configured");
    }
}
