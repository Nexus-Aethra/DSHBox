//! One container identifier, resolved the same way everywhere.
//!
//! A container's id is `container-<timestamp>`, which nobody types from
//! memory. An agent or a person reaches for what they can see: the name they
//! created the container with, or the bare timestamp part of the id. This
//! module is the single resolver both the daemon verbs (over a runtime root)
//! and list-holding callers such as the CLI (over a scanned container list)
//! share, so the match rules can never drift apart again.
//!
//! Safety: this is the only place that touches the filesystem with a caller's
//! raw string, so the join gate runs before any join. `is_safe_identifier`
//! permits dots, and `instances/..` is a real directory: joined blindly it
//! returns the runtime root, which is every container on the machine. Dots
//! are legitimate inside an id, so a segment that is empty or all dots is
//! refused rather than sanitized. Name matching returns directories taken
//! from scanned records and never joins the raw string.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::DshContainer;

/// A resolved container: canonical id and its on-disk directory.
#[derive(Debug, Clone)]
pub struct ResolvedContainer {
    pub id: String,
    pub directory: PathBuf,
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
    let scanned = crate::scan_containers(&root.to_string_lossy())
        .map_err(|error| format!("cannot scan containers: {error}"))?;
    Ok(scanned.values().cloned().collect())
}

/// Resolve how a caller refers to a container, inside a runtime root the
/// caller already holds.
///
/// Accepts three spellings: the full id (`container-1730...`), the id without
/// its `container-` prefix (exact, and the `container-<ts>-` prefix for the
/// sealed-template shape that carries a task id after it), and the name the
/// user created the container with. A name held by two containers is an
/// error rather than a first match: picking silently would send a write to
/// whichever one the directory scan happened to visit first.
pub fn resolve_container_in_root(
    root: &Path,
    raw: &str,
) -> Result<ResolvedContainer, String> {
    check_shape(raw)?;
    match fast_lookup(root, raw)? {
        Some(found) => Ok(found),
        None => {
            let containers = containers_in(root)?;
            let found = match_container(containers.as_slice(), raw)?;
            Ok(ResolvedContainer {
                id: found.id.clone(),
                directory: PathBuf::from(&found.directory),
            })
        }
    }
}

/// Match one reference against an in-memory container list, the way a
/// caller that already scanned or fetched the list resolves it. Same rules,
/// same errors as [`resolve_container_in_root`]; the CLI shares this so its
/// duplicate-name tie-breaking cannot drift from the daemon's.
pub fn match_container<'a>(
    containers: &'a [DshContainer],
    raw: &str,
) -> Result<&'a DshContainer, String> {
    check_shape(raw)?;
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

    matched.ok_or_else(|| {
        // An unresolvable reference has to say what would have worked.
        // "container not found: x" leaves the caller guessing whether the
        // store, the name, or the id format is wrong; naming the containers
        // that do exist ends the guess, which is the difference between an
        // agent recovering and an agent retrying the same typo four times.
        let mut labels: Vec<&str> = containers.iter().map(|c| c.name.as_str()).collect();
        labels.sort_unstable();
        if labels.is_empty() {
            format!("no container named {raw:?}, and there are no containers yet")
        } else {
            format!("no container named {raw:?}; the containers are: {}", labels.join(", "))
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        let found = resolve_container_in_root(&root, "container-1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000");
        assert!(found.directory.is_dir());
    }

    #[test]
    fn a_name_resolves_to_the_same_container() {
        let root = scratch("name");
        make_container(&root, "container-1730000001", "alpha");
        let found = resolve_container_in_root(&root, "alpha").unwrap();
        assert_eq!(found.id, "container-1730000001");
        // The list-based API the CLI shares answers the same way.
        let listed = vec![crate::scan_containers(&root.to_string_lossy())
            .unwrap()
            .remove("container-1730000001")
            .unwrap()];
        assert_eq!(match_container(&listed, "alpha").unwrap().id, "container-1730000001");
    }

    #[test]
    fn the_bare_timestamp_part_resolves() {
        let root = scratch("bare");
        make_container(&root, "container-1730000002", "alpha");
        let found = resolve_container_in_root(&root, "1730000002").unwrap();
        assert_eq!(found.id, "container-1730000002");
    }

    #[test]
    fn an_unknown_reference_lists_the_containers_that_exist() {
        let root = scratch("unknown");
        make_container(&root, "container-1730000003", "alpha");
        make_container(&root, "container-1730000004", "beta");
        let error = resolve_container_in_root(&root, "nope").unwrap_err();
        assert!(error.contains("alpha"), "{error}");
        assert!(error.contains("beta"), "{error}");
    }

    #[test]
    fn an_empty_store_says_so_instead_of_listing_nothing() {
        let root = scratch("empty");
        let error = resolve_container_in_root(&root, "anything").unwrap_err();
        assert!(error.contains("no containers yet"), "{error}");
    }

    #[test]
    fn two_containers_sharing_a_name_are_ambiguous_not_first_match() {
        let root = scratch("dupe");
        make_container(&root, "container-1730000005", "same");
        make_container(&root, "container-1730000006", "same");
        let error = resolve_container_in_root(&root, "same").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("container-1730000005"), "{error}");
        // The list-based API is ambiguity-first as well: it must not quietly
        // pick the row a directory scan happened to visit first.
        let listed = crate::scan_containers(&root.to_string_lossy())
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        assert!(match_container(&listed, "same").is_err());
    }

    #[test]
    fn a_directory_without_container_json_is_not_resolvable() {
        let root = scratch("broken");
        fs::create_dir_all(root.join("instances/container-1730000008")).unwrap();
        let error = resolve_container_in_root(&root, "container-1730000008").unwrap_err();
        assert!(error.contains("no container named"), "{error}");
    }

    #[test]
    fn a_sealed_style_timestamp_prefix_resolves() {
        let root = scratch("sealed-prefix");
        make_container(&root, "container-1730000000-4f2a", "sealed one");
        make_container(&root, "container-1730000001-9b3c", "sealed two");
        let found = resolve_container_in_root(&root, "1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000-4f2a");
        // Two ids sharing the timestamp prefix are an ambiguity, not a guess.
        make_container(&root, "container-1730000000-77ee", "sealed three");
        let error = resolve_container_in_root(&root, "1730000000").unwrap_err();
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
            let error = resolve_container_in_root(&root, attempt).unwrap_err();
            assert!(error.contains("no container named"), "{attempt}: {error}");
        }
    }

    #[test]
    fn a_blank_reference_is_rejected_and_a_spaced_name_resolves() {
        let root = scratch("shape");
        for attempt in ["", "   "] {
            let error = resolve_container_in_root(&root, attempt).unwrap_err();
            assert!(error.contains("expected a container"), "{attempt:?}: {error}");
        }
        // Names are only length-checked at creation: a legal name can carry
        // spaces, slashes or any Unicode, and matching it must not be
        // narrowed by the charset the id path-join needs.
        make_container(&root, "container-1730000009", "dev box/开发");
        let found = resolve_container_in_root(&root, "dev box/开发").unwrap();
        assert_eq!(found.id, "container-1730000009");
    }
}