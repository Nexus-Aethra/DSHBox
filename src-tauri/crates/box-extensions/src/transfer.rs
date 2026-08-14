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

/// Locates the single extension root inside an extracted directory: the
/// directory itself when it holds `SKILL.md`/`package.json`, otherwise its
/// only child directory.
pub fn archive_content_root(destination: &Path) -> Result<PathBuf, String> {
    if destination.join("SKILL.md").is_file() || destination.join("package.json").is_file() {
        return Ok(destination.to_path_buf());
    }
    let entries = fs::read_dir(destination)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    if entries.len() == 1 && entries[0].path().is_dir() {
        Ok(entries[0].path())
    } else {
        Err("tarball must contain one extension directory".to_owned())
    }
}

/// Appends a directory tree to an in-progress gzip tarball, skipping `.git`
/// and `node_modules` so exports stay small and reproducible.
pub fn append_plugin_archive(
    archive: &mut tar::Builder<GzEncoder<fs::File>>,
    root: &Path,
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
            append_plugin_archive(archive, root, &path, &output)?;
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
    append_plugin_archive(&mut archive, source, source, Path::new("extension"))?;
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
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-transfer-{name}-{}", crate::now_seconds()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn copy_skips_git_and_node_modules() {
        let root = sandbox("copy");
        let source = root.join("source");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("package.json"), "{}").unwrap();
        fs::write(source.join(".git/HEAD"), "ref").unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(source.join("src/index.ts"), "export").unwrap();
        let target = root.join("target");
        copy_extension_source(&source, &target).unwrap();
        assert!(target.join("package.json").is_file());
        assert!(target.join("src/index.ts").is_file());
        assert!(!target.join(".git").exists());
        assert!(!target.join("node_modules").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn export_and_extract_round_trip_preserves_content() {
        let root = sandbox("roundtrip");
        let plugin = root.join("plugin");
        fs::create_dir_all(plugin.join(".git")).unwrap();
        fs::create_dir_all(plugin.join("lib")).unwrap();
        fs::write(plugin.join("package.json"), r#"{"name":"demo","version":"1.0.0"}"#)
            .unwrap();
        fs::write(plugin.join("lib/main.js"), "console.log(1)").unwrap();
        fs::write(plugin.join(".git/config"), "ignored").unwrap();
        let tarball = root.join("demo.tar.gz");
        export_extension_directory(&plugin, &tarball).unwrap();
        let extracted = root.join("extracted");
        fs::create_dir_all(&extracted).unwrap();
        extract_extension_tarball(&tarball, &extracted).unwrap();
        let content_root = archive_content_root(&extracted).unwrap();
        assert!(content_root.join("package.json").is_file());
        assert!(content_root.join("lib/main.js").is_file());
        assert!(!content_root.join(".git").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_conflict_keeps_bracketed_names_and_overwrite_keeps_original() {
        let entries = vec![RepositoryExtension {
            id: "a".to_owned(),
            kind: ExtensionKind::Plugin,
            name: "demo".to_owned(),
            version: None,
            description: None,
            content_digest: "d".to_owned(),
            source_path: "p".to_owned(),
            imported_at: 0,
            diagnostic: None,
            source: None,
        }];
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Plugin, "demo", "keep"),
            "demo (2)"
        );
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Plugin, "demo", "overwrite"),
            "demo"
        );
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Skill, "demo", "keep"),
            "demo"
        );
    }
}
