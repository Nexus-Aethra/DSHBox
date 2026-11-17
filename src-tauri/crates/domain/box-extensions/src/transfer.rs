//! Extension transfer primitives: tarball packing/unpacking, directory
//! copies, and name-clash resolution. Pure file operations so the host can
//! orchestrate them inside tasks without pulling a GUI framework into the
//! crate.

use crate::{ExtensionKind, RepositoryExtension};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use xz2::read::XzDecoder;

/// Recursively copies a directory while skipping VCS metadata and installed
/// dependencies (`.git`, `node_modules`) that would bloat exports or leak
/// into repository storage.
pub fn copy_extension_source(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        let target = destination.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            copy_extension_source(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Extracts a `.tar`, `.tar.gz`, `.tgz`, or `.tar.xz` archive into
/// `destination`, refusing absolute paths and `..` traversal entries.
pub fn extract_extension_tarball(archive: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|error| format!("cannot open tarball: {error}"))?;
    let name = archive.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        unpack_tar(tar::Archive::new(GzDecoder::new(file)), destination)
    } else if name.ends_with(".tar.xz") {
        unpack_tar(tar::Archive::new(XzDecoder::new(file)), destination)
    } else if name.ends_with(".tar") {
        unpack_tar(tar::Archive::new(file), destination)
    } else {
        Err("supported archives are .tar, .tar.gz, .tgz, and .tar.xz".to_owned())
    }
}

fn unpack_tar<R: Read>(mut archive: tar::Archive<R>, destination: &Path) -> Result<(), String> {
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot read tarball: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("cannot read tarball entry: {error}"))?;
        let path = entry.path().map_err(|error| error.to_string())?;
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("tarball contains an unsafe path".to_owned());
        }
        if !entry
            .unpack_in(destination)
            .map_err(|error| format!("cannot extract tarball: {error}"))?
        {
            return Err("tarball entry escaped the destination".to_owned());
        }
    }
    Ok(())
}

/// Maximum number of subdirectory levels searched below an import
/// destination when locating the extension root.
pub const MAX_EXTENSION_ROOT_DEPTH: usize = 2;

/// Locates the extension root inside an import destination: the directory
/// itself when it holds `SKILL.md`/`package.json`, otherwise a layered
/// (depth-first) search down to `MAX_EXTENSION_ROOT_DEPTH` levels below.
/// The shallowest match wins; within a level, `read_dir` order decides.
/// Noise directories (VCS, installed dependencies, build output) are
/// skipped so a monorepo checkout does not accidentally match a vendored
/// copy.
pub fn locate_extension_root(directory: &Path) -> Result<PathBuf, String> {
    let mut depth = 0usize;
    let mut frontier = vec![directory.to_path_buf()];
    loop {
        let mut next = Vec::new();
        for candidate in &frontier {
            if candidate.join("SKILL.md").is_file()
                || candidate.join("package.json").is_file()
            {
                return Ok(candidate.clone());
            }
            if depth < MAX_EXTENSION_ROOT_DEPTH {
                let entries = fs::read_dir(candidate).map_err(|error| error.to_string())?;
                for entry in entries.filter_map(Result::ok) {
                    let name = entry.file_name();
                    if matches!(
                        name.to_str(),
                        Some(".git" | "node_modules" | "dist" | "build" | ".cache" | ".dsh")
                    ) {
                        continue;
                    }
                    if entry
                        .file_type()
                        .map(|kind| kind.is_dir())
                        .unwrap_or(false)
                    {
                        next.push(entry.path());
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
        depth += 1;
    }
    Err(format!(
        "no extension root found (looked up to {MAX_EXTENSION_ROOT_DEPTH} level(s) below the import directory)"
    ))
}

/// Locates the single extension root inside an extracted directory. The
/// directory itself when it holds `SKILL.md`/`package.json`, otherwise a
/// depth-limited search of its subdirectories (see `locate_extension_root`).
pub fn archive_content_root(destination: &Path) -> Result<PathBuf, String> {
    locate_extension_root(destination)
}

/// Appends a directory tree to an in-progress gzip tarball, skipping `.git`
/// and `node_modules` so exports stay small and reproducible.
pub fn append_plugin_archive(
    archive: &mut tar::Builder<GzEncoder<fs::File>>,
    directory: &Path,
    target: &Path,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        let path = entry.path();
        let output = target.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            archive
                .append_dir(&output, &path)
                .map_err(|error| error.to_string())?;
            append_plugin_archive(archive, &path, &output)?;
        } else if kind.is_file() {
            archive
                .append_path_with_name(&path, &output)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Packs an extension directory into a gzip tarball at `destination`.
pub fn export_extension_directory(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("plugin export destination must end in .tar.gz".to_owned());
    }
    fs::create_dir_all(destination.parent().ok_or("plugin export has no parent")?)
        .map_err(|error| error.to_string())?;
    let output = fs::File::create(destination)
        .map_err(|error| format!("cannot create extension tarball: {error}"))?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_plugin_archive(&mut archive, source, Path::new("extension"))?;
    archive.finish().map_err(|error| error.to_string())?;
    Ok(())
}

/// Picks the repository name for an imported extension when its name clashes
/// with an existing entry of the same kind: "overwrite" keeps the imported
/// name (the caller drops the stale entries first), "keep" appends a
/// bracketed counter (`name (2)`, `name (3)`, ...).
pub fn resolve_conflict_name(
    repository: &[RepositoryExtension],
    kind: &ExtensionKind,
    real_name: &str,
    conflict: &str,
) -> String {
    let clashes =
        |candidate: &str| repository.iter().any(|entry| entry.kind == *kind && entry.name == candidate);
    if clashes(real_name) && conflict != "overwrite" {
        let mut n = 2;
        while clashes(&format!("{real_name} ({n})")) {
            n += 1;
        }
        format!("{real_name} ({n})")
    } else {
        real_name.to_owned()
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;
