//! Moving resource payloads in and out of a container, with the safety rules
//! that makes that acceptable: nothing escapes the container root, secrets land
//! `0600`, and an existing destination is never replaced unless asked for.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::kinds::{Conflict, Shape};

pub mod parts;
pub mod section;
pub mod tree;

// The transfer surface the rest of the crate uses. Before this module became
// a directory each of these was a sibling item reachable as `transfer::x`;
// they are re-exported so no caller had to change. Everything else stays
// `pub(super)` and is reached as `super::x` from inside the directory.
pub use parts::{extract_parts, inject_parts, payload_parts, PayloadPart};
pub use section::{entries_at, write_section};
pub use tree::{copy_tree, pack_tar, safe_join, tree_digest, tree_stats, unpack_tar};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub bytes: u64,
    pub files: u64,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Injected {
    pub bytes: u64,
    pub files: u64,
    /// Entries the payload replaced, for the task log.
    pub replaced: Vec<String>,
    /// Entries the payload added.
    pub added: Vec<String>,
}

/// Copy a container path into a payload directory, replacing whatever was
/// there. With `selection` only that one entry (relative to the kind's path) is
/// taken, which is how a single chat session is extracted out of many.
pub fn extract(
    container_root: &Path,
    rel: &str,
    selection: Option<&str>,
    payload_dir: &Path,
    secret: bool,
) -> Result<Extracted, String> {
    extract_parts(
        container_root,
        &[PayloadPart {
            path: rel.to_owned(),
            section: Vec::new(),
            slot: None,
        }],
        selection,
        payload_dir,
        secret,
    )
}

fn extract_path(
    container_root: &Path,
    rel: &str,
    selection: Option<&str>,
    target: &Path,
) -> Result<(u64, u64), String> {
    let source = safe_join(container_root, rel)?;
    let selected = match selection {
        Some(entry) => Some(safe_join(&source, entry)?),
        None => None,
    };
    if let Some(node) = &selected {
        if fs::symlink_metadata(node).is_err() {
            return Err(format!("{} does not exist", node.display()));
        }
    } else if !source.exists() {
        return Err(format!("nothing to extract: {} does not exist", source.display()));
    }
    fs::create_dir_all(target).map_err(|error| error.to_string())?;
    match (&selected, selection) {
        (Some(node), Some(entry)) => {
            let destination = safe_join(target, entry)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            copy_tree(node, &destination, container_root)
        }
        _ => {
            if source.is_dir() {
                copy_tree(&source, target, container_root)
            } else {
                let name = source
                    .file_name()
                    .ok_or_else(|| format!("{} has no file name", source.display()))?;
                copy_tree(&source, &target.join(name), container_root)
            }
        }
    }
}

/// Copy a payload into a container path.
///
/// Entry-shaped kinds (`Shape::Entries`) compare entries at the kind's depth:
/// `Refuse` reports every conflict before touching anything, `Merge` replaces
/// only the entries the payload carries, and `Overwrite` empties the kind's
/// path first. Opaque kinds treat the destination as one value: `Merge` merges
/// YAML maps or directories, `Overwrite` replaces it, `Refuse` refuses.
pub fn inject(
    payload_dir: &Path,
    container_root: &Path,
    rel: &str,
    conflict: Conflict,
    shape: Shape,
    entry_depth: u8,
    secret: bool,
) -> Result<Injected, String> {
    inject_parts(
        payload_dir,
        container_root,
        &[PayloadPart {
            path: rel.to_owned(),
            section: Vec::new(),
            slot: None,
        }],
        conflict,
        shape,
        entry_depth,
        secret,
    )
}
