//! Filesystem primitives the transfer stands on: safe path joining, tree
//! stats and digests, copies, permission tightening, and tar pack/unpack.
//!
//! Nothing here knows about payloads, parts, or YAML sections -- it is the
//! layer the other three modules sit on, and the only one that touches the
//! filesystem directly.

use super::*;

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

/// List a directory's entries, sorted. The sort is what makes the tar and the
/// digest reproducible: an unsorted listing would let the same tree produce
/// different bytes on different runs.
pub(super) fn read_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        entries.push(entry.path());
    }
    entries.sort();
    Ok(entries)
}
pub(super) fn collect_files(root: &Path, prefix: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
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
pub(super) fn restrict_permissions(path: &Path) {
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
