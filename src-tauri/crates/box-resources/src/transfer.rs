//! Moving resource payloads in and out of a container, with the safety rules
//! that makes that acceptable: nothing escapes the container root, secrets land
//! `0600`, and an existing destination is never replaced unless asked for.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::kinds::{Conflict, Shape};

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

/// Join `rel` onto `root`, refusing anything that could leave it: absolute
/// paths, `..`, and root/prefix components are all rejected outright rather
/// than normalized, because a resource path is always container-relative.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("resource path is empty".to_owned());
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            other => {
                return Err(format!(
                    "resource path `{rel}` must stay inside the container ({other:?} is not allowed)"
                ))
            }
        }
    }
    Ok(root.join(candidate))
}

/// Bytes and file count under `path`, without following symlinks (a symlink
/// out of the container must not make an extraction walk the host filesystem).
pub fn tree_stats(path: &Path) -> Result<(u64, u64), String> {
    let meta = fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if meta.is_symlink() {
        return Ok((0, 1));
    }
    if meta.is_file() {
        return Ok((meta.len(), 1));
    }
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry in fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        let (child_bytes, child_files) = tree_stats(&entry.path())?;
        bytes += child_bytes;
        files += child_files;
    }
    Ok((bytes, files))
}

/// Content digest of a payload tree: path names plus file bytes, so two
/// extractions of the same state agree and a changed file changes the digest.
pub fn tree_digest(path: &Path) -> Result<String, String> {
    let mut entries = Vec::new();
    collect_files(path, Path::new(""), &mut entries)?;
    entries.sort();
    let mut hash: u64 = 0xcbf29ce484222325;
    for (relative, file) in entries {
        for byte in relative.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        if let Ok(bytes) = fs::read(&file) {
            for byte in bytes {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    Ok(format!("{hash:016x}"))
}

fn collect_files(root: &Path, prefix: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let meta = fs::symlink_metadata(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let relative = prefix.to_string_lossy().to_string();
    if meta.is_symlink() {
        out.push((format!("{relative}@link"), root.to_path_buf()));
        return Ok(());
    }
    if meta.is_file() {
        out.push((relative, root.to_path_buf()));
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        collect_files(&entry.path(), &prefix.join(entry.file_name()), out)?;
    }
    Ok(())
}

/// Copy a tree, creating directories, preserving modes, and refusing a symlink
/// whose target leaves `root`.
pub fn copy_tree(from: &Path, to: &Path, root: &Path) -> Result<(u64, u64), String> {
    let meta = fs::symlink_metadata(from).map_err(|error| format!("{}: {error}", from.display()))?;
    if meta.is_symlink() {
        let target = fs::read_link(from).map_err(|error| error.to_string())?;
        let resolved = if target.is_absolute() {
            target.clone()
        } else {
            from.parent().unwrap_or(root).join(&target)
        };
        if !resolved.starts_with(root) {
            return Err(format!(
                "{} is a symlink out of the container (-> {}); refusing to copy it",
                from.display(),
                target.display()
            ));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, to)
            .map_err(|error| format!("{}: {error}", to.display()))?;
        return Ok((0, 1));
    }
    if meta.is_file() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::copy(from, to).map_err(|error| format!("{}: {error}", to.display()))?;
        return Ok((meta.len(), 1));
    }
    fs::create_dir_all(to).map_err(|error| format!("{}: {error}", to.display()))?;
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry in fs::read_dir(from).map_err(|error| format!("{}: {error}", from.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        let (child_bytes, child_files) = copy_tree(&entry.path(), &to.join(entry.file_name()), root)?;
        bytes += child_bytes;
        files += child_files;
    }
    Ok((bytes, files))
}

/// Tighten a payload that carries a secret: files `0600`, directories `0700`.
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_symlink() {
            return;
        }
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    restrict_permissions(&entry.path());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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

    if payload_dir.exists() {
        fs::remove_dir_all(payload_dir).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(payload_dir).map_err(|error| error.to_string())?;

    let (bytes, files) = match (&selected, selection) {
        (Some(node), Some(entry)) => {
            let target = safe_join(payload_dir, entry)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            copy_tree(node, &target, container_root)?
        }
        _ => {
            if source.is_dir() {
                copy_tree(&source, payload_dir, container_root)?
            } else {
                let name = source
                    .file_name()
                    .ok_or_else(|| format!("{} has no file name", source.display()))?;
                copy_tree(&source, &payload_dir.join(name), container_root)?
            }
        }
    };
    if secret {
        restrict_permissions(payload_dir);
    }
    Ok(Extracted {
        bytes,
        files,
        digest: tree_digest(payload_dir)?,
    })
}

/// The independent entries of a tree at the kind's depth, as `(relative, path)`
/// pairs. A file at any level is an entry in its own right, so a stray file
/// beside the entries is carried instead of being silently dropped.
pub fn entries_at(root: &Path, depth: u8) -> Result<Vec<(String, PathBuf)>, String> {
    let mut entries = Vec::new();
    walk_entries(root, Path::new(""), depth.max(1), &mut entries)?;
    entries.sort();
    Ok(entries)
}

fn walk_entries(
    dir: &Path,
    prefix: &Path,
    depth: u8,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    for entry in read_entries(dir)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let relative = prefix.join(name);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if depth <= 1 || !entry.is_dir() {
            out.push((relative_text, entry));
        } else {
            walk_entries(&entry, &relative, depth - 1, out)?;
        }
    }
    Ok(())
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

/// Merge two YAML documents: mappings merge key by key, the incoming value wins
/// on a scalar or sequence conflict.
pub fn merge_yaml(target: &str, incoming: &str) -> Result<String, String> {
    let base: serde_yaml::Value =
        serde_yaml::from_str(target).map_err(|error| format!("invalid YAML at destination: {error}"))?;
    let overlay: serde_yaml::Value =
        serde_yaml::from_str(incoming).map_err(|error| format!("invalid YAML payload: {error}"))?;
    let merged = merge_value(base, overlay);
    serde_yaml::to_string(&merged).map_err(|error| error.to_string())
}

fn merge_value(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Mapping(base)
        }
        (_, overlay) => overlay,
    }
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

fn read_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
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

/// Pack a payload directory as a gzipped tarball, for moving a resource between
/// machines.
pub fn pack_tar(payload_dir: &Path, out: &Path) -> Result<(), String> {
    let file = fs::File::create(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let entries = read_entries(payload_dir)?;
    for entry in entries {
        let name = entry.file_name().ok_or("payload entry has no name")?;
        if entry.is_dir() {
            builder
                .append_dir_all(name, &entry)
                .map_err(|error| error.to_string())?;
        } else {
            builder
                .append_path_with_name(&entry, name)
                .map_err(|error| error.to_string())?;
        }
    }
    let encoder = builder.into_inner().map_err(|error| error.to_string())?;
    encoder.finish().map_err(|error| error.to_string())?;
    Ok(())
}

/// Unpack a tarball into a payload directory, rejecting entries that try to
/// leave it.
pub fn unpack_tar(input: &Path, payload_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(input).map_err(|error| format!("{}: {error}", input.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    if payload_dir.exists() {
        fs::remove_dir_all(payload_dir).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(payload_dir).map_err(|error| error.to_string())?;
    for entry in archive.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let relative = entry.path().map_err(|error| error.to_string())?.to_path_buf();
        let target = safe_join(payload_dir, &relative.to_string_lossy())?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target).map_err(|error| error.to_string())?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                return Err(format!(
                    "{} contains a link ({}); resource archives must be plain files",
                    input.display(),
                    relative.display()
                ))
            }
            _ => {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
                let mut out = fs::File::create(&target).map_err(|error| error.to_string())?;
                out.write_all(&bytes).map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}
