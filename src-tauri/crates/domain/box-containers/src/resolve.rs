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
#[path = "resolve_tests.rs"]
mod tests;
