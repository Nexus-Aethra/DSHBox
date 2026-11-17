//! The payload layout: what a transferred resource looks like on disk.
//!
//! A payload is one or more parts. Part 0 is written at the payload root,
//! unchanged, so copies taken before parts existed still inject; later
//! parts live under `part-<i>/`, and `parts.json` describes the whole set
//! so a payload explains itself rather than trusting the kind installed now.

use super::*;
use super::section::{entries_at, extract_section, inject_section, merge_yaml, SECTION_FILE};
use super::tree::restrict_permissions;

/// One part of a payload: where it goes back, and whether it is a whole path or
/// one key path inside a YAML document there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadPart {
    pub path: String,
    #[serde(default)]
    pub section: Vec<String>,
    /// The version slot this part came from, when it came from one. An
    /// injection re-picks the destination's own part for that slot, so a copy
    /// taken from a newer Harness layout lands where the older container keeps
    /// the same state rather than cloning its source's path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// What the payload says about itself. It is written whenever a payload has
/// more than one part, so an injection never has to guess from the kind that
/// happens to be installed now.
const PARTS_FILE: &str = "parts.json";

/// A payload with no manifest is the one path the caller named: a copy taken
/// before kinds had parts.
pub fn payload_parts(payload_dir: &Path, fallback_path: &str) -> Vec<PayloadPart> {
    read_parts_manifest(payload_dir).unwrap_or_else(|| {
        vec![PayloadPart {
            path: fallback_path.to_owned(),
            section: Vec::new(),
            slot: None,
        }]
    })
}

pub fn read_parts_manifest(payload_dir: &Path) -> Option<Vec<PayloadPart>> {
    let text = fs::read_to_string(payload_dir.join(PARTS_FILE)).ok()?;
    serde_json::from_str::<Vec<PayloadPart>>(&text)
        .ok()
        .filter(|parts| !parts.is_empty())
}

fn write_parts_manifest(payload_dir: &Path, parts: &[PayloadPart]) -> Result<(), String> {
    let body = serde_json::to_string_pretty(parts).map_err(|error| error.to_string())?;
    fs::write(payload_dir.join(PARTS_FILE), body).map_err(|error| error.to_string())
}

/// Where one part lives inside the payload. A single-part payload keeps the
/// layout every copy has always had (the path itself at the root); once there
/// is more than one, each part gets a directory of its own so a whole-file part
/// is still one file to the code that reads it.
fn part_dir(payload_dir: &Path, index: usize, many: bool) -> PathBuf {
    if many || index > 0 {
        payload_dir.join(format!("part-{index}"))
    } else {
        payload_dir.to_path_buf()
    }
}

/// Extract every part of a kind into one payload directory. A part that is a
/// section of a YAML document travels as that subtree alone, so the rest of the
/// file (another plugin's settings) stays where it is.
pub fn extract_parts(
    container_root: &Path,
    parts: &[PayloadPart],
    selection: Option<&str>,
    payload_dir: &Path,
    secret: bool,
) -> Result<Extracted, String> {
    if parts.is_empty() {
        return Err("this resource has nothing to extract".to_owned());
    }
    if payload_dir.exists() {
        fs::remove_dir_all(payload_dir).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(payload_dir).map_err(|error| error.to_string())?;

    let many = parts.len() > 1;
    let mut bytes = 0u64;
    let mut files = 0u64;
    for (index, part) in parts.iter().enumerate() {
        // A selection is one entry of the primary part (a single session out of
        // a history), so it only ever applies to the first.
        let selection = if index == 0 { selection } else { None };
        let target = part_dir(payload_dir, index, many);
        let (part_bytes, part_files) = if part.section.is_empty() {
            extract_path(container_root, &part.path, selection, &target)?
        } else {
            extract_section(container_root, part, &target)?
        };
        bytes += part_bytes;
        files += part_files;
    }
    if many {
        write_parts_manifest(payload_dir, parts)?;
    }
    if secret {
        restrict_permissions(payload_dir);
    }
    Ok(Extracted {
        bytes,
        files,
        digest: tree_digest(payload_dir)?,
    })
}

/// Inject every part of a payload. A part that is a section of a YAML document
/// is merged back into that document at the same key path, so the rest of the
/// file keeps whatever it had.
pub fn inject_parts(
    payload_dir: &Path,
    container_root: &Path,
    parts: &[PayloadPart],
    conflict: Conflict,
    shape: Shape,
    entry_depth: u8,
    secret: bool,
) -> Result<Injected, String> {
    let mut result = Injected {
        bytes: 0,
        files: 0,
        replaced: Vec::new(),
        added: Vec::new(),
    };
    let many = parts.len() > 1;
    for (index, part) in parts.iter().enumerate() {
        let source = part_dir(payload_dir, index, many);
        if !source.exists() {
            return Err(format!(
                "payload is missing part {index} ({})",
                part.path
            ));
        }
        let injected = if part.section.is_empty() {
            inject_one(&source, container_root, part, conflict, shape, entry_depth, secret)?
        } else {
            inject_section(&source.join(SECTION_FILE), container_root, part, conflict)?
        };
        result.bytes += injected.bytes;
        result.files += injected.files;
        result.replaced.extend(injected.replaced);
        result.added.extend(injected.added);
    }
    Ok(result)
}

fn inject_one(
    payload_dir: &Path,
    container_root: &Path,
    part: &PayloadPart,
    conflict: Conflict,
    shape: Shape,
    entry_depth: u8,
    secret: bool,
) -> Result<Injected, String> {
    let rel = part.path.as_str();
    let dest = safe_join(container_root, rel)?;
    let mut replaced = Vec::new();
    let mut added = Vec::new();
    let (bytes, files) = match shape {
        Shape::Entries => {
            let payload_entries = entries_at(payload_dir, entry_depth)?;
            if payload_entries.is_empty() {
                return Err(format!("payload {} holds no entries", payload_dir.display()));
            }
            if conflict == Conflict::Overwrite {
                if fs::symlink_metadata(&dest).is_ok() {
                    remove_path(&dest)?;
                }
            }
            if conflict == Conflict::Refuse {
                // Report everything that clashes before writing anything: a
                // half-applied injection is worse than a refusal.
                let clashes: Vec<String> = payload_entries
                    .iter()
                    .filter(|(entry, _)| fs::symlink_metadata(dest.join(entry)).is_ok())
                    .map(|(entry, _)| entry.clone())
                    .collect();
                if !clashes.is_empty() {
                    return Err(format!(
                        "{} already holds {}; pass --overwrite to replace the kind or --merge to \
                         inject only what is missing",
                        dest.display(),
                        clashes.join(", ")
                    ));
                }
            }
            fs::create_dir_all(&dest).map_err(|error| error.to_string())?;
            let mut total = (0u64, 0u64);
            for (entry, source) in payload_entries {
                let target = dest.join(&entry);
                if fs::symlink_metadata(&target).is_ok() {
                    remove_path(&target)?;
                    replaced.push(entry);
                } else {
                    added.push(entry);
                }
                let (child_bytes, child_files) = copy_tree(&source, &target, payload_dir)?;
                total.0 += child_bytes;
                total.1 += child_files;
            }
            total
        }
        Shape::Opaque => {
            let (bytes, files) = tree_stats(payload_dir)?;
            if fs::symlink_metadata(&dest).is_ok() {
                match conflict {
                    Conflict::Refuse => {
                        return Err(format!(
                            "{} already exists; pass --overwrite to replace it or --merge to \
                             merge into it",
                            dest.display()
                        ))
                    }
                    Conflict::Merge => {
                        merge_opaque(payload_dir, &dest)?;
                        if secret {
                            restrict_permissions(&dest);
                        }
                        replaced.push(rel.to_owned());
                        return Ok(Injected { bytes, files, replaced, added });
                    }
                    Conflict::Overwrite => {
                        remove_path(&dest)?;
                        replaced.push(rel.to_owned());
                    }
                }
            } else {
                added.push(rel.to_owned());
            }
            let single = single_payload_file(payload_dir)?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            copy_tree(&single, &dest, payload_dir)?
        }
    };
    if secret {
        restrict_permissions(&dest);
    }
    Ok(Injected {
        bytes,
        files,
        replaced,
        added,
    })
}

/// A one-file payload (`credentials`) keeps its own file name in the payload
/// directory; the destination is the file itself.
fn single_payload_file(payload_dir: &Path) -> Result<PathBuf, String> {
    let entries = read_entries(payload_dir)?;
    match entries.len() {
        1 => Ok(entries[0].clone()),
        0 => Err(format!("payload {} is empty", payload_dir.display())),
        _ => Err(format!(
            "payload {} holds {} entries but this kind is a single file",
            payload_dir.display(),
            entries.len()
        )),
    }
}

fn merge_opaque(payload_dir: &Path, dest: &Path) -> Result<(), String> {
    let incoming = single_payload_file(payload_dir)?;
    let is_yaml = |path: &Path| {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yaml" | "yml")
        )
    };
    if is_yaml(&incoming) && is_yaml(dest) {
        let target_text = fs::read_to_string(dest).map_err(|error| error.to_string())?;
        let incoming_text = fs::read_to_string(&incoming).map_err(|error| error.to_string())?;
        let merged = merge_yaml(&target_text, &incoming_text)?;
        fs::write(dest, merged).map_err(|error| error.to_string())?;
        return Ok(());
    }
    if dest.is_dir() && incoming.is_dir() {
        let mut entries = Vec::new();
        collect_entries(&incoming, &mut entries);
        for entry in entries {
            let relative = entry
                .strip_prefix(&incoming)
                .map_err(|_| "payload entry escaped its root".to_owned())?;
            let target = dest.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            if fs::symlink_metadata(&target).is_ok() {
                remove_path(&target)?;
            }
            copy_tree(&entry, &target, &incoming)?;
        }
        return Ok(());
    }
    Err(format!(
        "cannot merge {} into {}: merging only applies to YAML maps and directories",
        incoming.display(),
        dest.display()
    ))
}

fn collect_entries(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_entries(&path, out);
        } else {
            out.push(path);
        }
    }
}

pub(super) fn read_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        entries.push(entry.path());
    }
    entries.sort();
    Ok(entries)
}

fn remove_path(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if meta.is_dir() {
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    } else {
        fs::remove_file(path).map_err(|error| error.to_string())
    }
}
