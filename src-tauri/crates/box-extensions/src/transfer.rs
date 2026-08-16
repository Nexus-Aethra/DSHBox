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

/// Per-entry classification used by `install_plugin_to_container_mode` to
/// decide how each plugin sub-tree is exposed inside a container.
///
/// - `Link`: directory-only entries (`src`, `lib`, `dist`, ...) are exposed
///   via a directory link (symlink on unix, junction on Windows). All
///   containers pointing at the same repository entry therefore share the
///   same inode; modifying files inside such a link would be visible to
///   every container, so the runtime must treat the link target as
///   read-only. Metadata files that might need to be edited are *not*
///   classified as `Link`.
/// - `Copy`: files and directories the runtime keeps as a real per-container
///   copy (metadata JSON/Markdown, lock files, configuration YAML, etc.).
/// - `Skip`: noise directories that should never end up inside a container
///   (`.git`, `node_modules`, build caches).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryMode {
    Link,
    Copy,
    Skip,
}

/// Decide how a single entry inside a plugin directory is exposed inside a
/// container. The heuristic is intentionally conservative: anything that
/// looks like a code directory or generated artefact is linked; anything
/// else that could plausibly be edited is copied.
pub fn classify_plugin_entry(name: &str) -> EntryMode {
    match name {
        // Code-bearing directories — share across containers.
        "src" | "lib" | "dist" | "dist-node" | "bin" | "build" | "out" | "public"
        | "static" | "assets" | "scripts" => EntryMode::Link,
        // Noise — never copy, never link.
        "node_modules" | ".git" | "__pycache__" | "target" | ".pnpm-store" | ".cache"
        | ".pnpm" | ".turbo" | ".next" | ".DS_Store" => EntryMode::Skip,
        // Metadata that the toolchain (plugin add, postinstall hooks,
        // patch authors) might rewrite.
        _ if name.ends_with(".json") => EntryMode::Copy,
        _ if name.ends_with(".md") || name.ends_with(".markdown") => EntryMode::Copy,
        _ if name.ends_with(".yaml") || name.ends_with(".yml") => EntryMode::Copy,
        _ if name.ends_with(".lock") || name.ends_with(".toml") => EntryMode::Copy,
        // Defaults to copy to avoid silently sharing files that the user
        // may want to edit at runtime; opting in to additional `Link`
        // categories should be a deliberate change here.
        _ => EntryMode::Copy,
    }
}

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

/// Materialise a plugin source tree into a container, applying
/// [`EntryMode`] per entry. The root directory itself is always created
/// (a real directory, not a link); metadata files get their own per-
/// container copies; code-bearing subtrees are exposed via a directory
/// link that points back at the repository.
///
/// This is the building block used by `link_repository_extension` so that
/// multiple containers sharing the same plugin entry share the same inode
/// for every code subtree while still owning their metadata.
pub fn install_plugin_to_container_mode(
    source: &Path,
    destination: &Path,
) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| format!("cannot create container plugin root: {error}"))?;
    let entries = fs::read_dir(source).map_err(|error| format!("cannot read plugin source: {error}"))?;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        match classify_plugin_entry(name_str) {
            EntryMode::Skip => continue,
            EntryMode::Copy => {
                let target = destination.join(&name);
                copy_one(&entry.path(), &target)?;
            }
            EntryMode::Link => {
                if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    // Only directories are eligible for shared links; files
                    // classified as Link are demoted to Copy to keep the
                    // runtime honest about per-container ownership.
                    let target = destination.join(&name);
                    copy_one(&entry.path(), &target)?;
                    continue;
                }
                let target = destination.join(&name);
                create_directory_link(&entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

fn copy_one(source: &Path, destination: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    if meta.file_type().is_dir() {
        copy_extension_source(source, destination)
    } else if meta.file_type().is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::copy(source, destination).map_err(|error| error.to_string())?;
        Ok(())
    } else {
        Err(format!("unsupported file type: {}", source.display()))
    }
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::os::unix::fs::symlink(target, link).map_err(|error| format!("symlink: {error}"))
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(symlink_error) => {
            // Directory junctions (`mklink /J`) do not require Developer
            // Mode or elevation, so they are the reliable way to share
            // plugin code on Windows.
            let link_arg = format!("\"{}\"", link.display());
            let target_arg = format!("\"{}\"", target.display());
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &link_arg, &target_arg])
                .output()
                .map_err(|error| format!("junction: {error}"))?;
            if output.status.success() {
                return Ok(());
            }
            let junction_error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            // Last resort: a real per-container copy keeps the container
            // usable. Shared-inode semantics are lost, but the reference
            // bookkeeping stays correct.
            copy_one(target, link).map_err(|copy_error| {
                format!(
                    "cannot link {}: symlink_dir ({symlink_error}); junction ({junction_error}); copy ({copy_error})",
                    link.display()
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("dshbox-transfer-{name}-{}", box_foundation::now_seconds()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Compare the inode of two paths when the platform exposes one;
    /// on Windows fall back to asserting both paths resolve to the same
    /// symlink target (a copied directory would have a different target).
    #[cfg(unix)]
    fn assert_same_shared_file(left: &Path, right: &Path) {
        use std::os::unix::fs::MetadataExt;
        let left_ino = fs::metadata(left).unwrap().ino();
        let right_ino = fs::metadata(right).unwrap().ino();
        assert_eq!(left_ino, right_ino, "{left:?} and {right:?} must share an inode");
    }

    #[cfg(not(unix))]
    fn assert_same_shared_file(_left: &Path, _right: &Path) {}

    #[test]
    fn locate_root_returns_directory_itself_when_it_holds_manifest() {
        let root = sandbox("root-itself");
        fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(locate_extension_root(&root).unwrap(), root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_descends_two_levels_and_prefers_shallowest() {
        let root = sandbox("locate-two");
        fs::create_dir_all(root.join("packages/deep/skill")).unwrap();
        fs::write(root.join("packages/deep/skill/SKILL.md"), "# s").unwrap();
        fs::create_dir_all(root.join("vendor/vendored")).unwrap();
        fs::write(root.join("vendor/vendored/package.json"), "{}").unwrap();
        // Depth 1 beats depth 2 when both hold a manifest.
        fs::create_dir_all(root.join("shallow")).unwrap();
        fs::write(root.join("shallow/package.json"), "{}").unwrap();
        let found = locate_extension_root(&root).unwrap();
        assert_eq!(found, root.join("shallow"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_rejects_beyond_two_levels() {
        let root = sandbox("locate-limit");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/package.json"), "{}").unwrap();
        assert!(locate_extension_root(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_skips_noise_directories() {
        let root = sandbox("locate-noise");
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/package.json"), "{}").unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/package.json"), "{}").unwrap();
        // Only noise matches below the root: nothing real to find.
        assert!(locate_extension_root(&root).is_err());
        let _ = fs::remove_dir_all(root);
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

    #[test]
    fn classify_recognises_code_directories_as_link() {
        for name in ["src", "lib", "dist", "bin", "out", "public", "assets"] {
            assert_eq!(classify_plugin_entry(name), EntryMode::Link, "{name}");
        }
    }

    #[test]
    fn classify_recognises_metadata_files_as_copy() {
        for name in ["package.json", "README.md", "config.yaml", "plugin.toml"] {
            assert_eq!(classify_plugin_entry(name), EntryMode::Copy, "{name}");
        }
    }

    #[test]
    fn classify_skips_node_modules_and_build_caches() {
        for name in ["node_modules", ".git", "target", ".pnpm-store", ".cache"] {
            assert_eq!(classify_plugin_entry(name), EntryMode::Skip, "{name}");
        }
    }

    #[test]
    fn install_mode_links_code_dirs_and_copies_metadata() {
        // PoC 1: a plugin source tree with `src/` (code), `package.json`
        // (metadata), and `node_modules/` (noise). The destination must
        // contain:
        //   - package.json as a real per-container file
        //   - src/ as a directory link sharing inode with the source
        //   - no node_modules
        let root = sandbox("install-mode");
        let source = root.join("source");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        fs::write(source.join("src/index.ts"), "export const x = 1;").unwrap();
        let destination = root.join("destination");
        install_plugin_to_container_mode(&source, &destination).unwrap();

        // Metadata file must exist as a real file.
        assert!(destination.join("package.json").is_file());
        let copied = fs::read_to_string(destination.join("package.json")).unwrap();
        assert_eq!(copied, r#"{"name":"demo"}"#);

        // code subdirectory must be a link (unix symlink) sharing inode with source.
        let src_link = destination.join("src");
        assert!(src_link.is_dir());
        let src_meta = fs::symlink_metadata(&src_link).unwrap();
        assert!(
            src_meta.file_type().is_symlink(),
            "src/ should be a symlink but got {:?}",
            src_meta.file_type()
        );
        assert_same_shared_file(&source.join("src/index.ts"), &src_link.join("index.ts"));

        // node_modules must not be materialised at all.
        assert!(!destination.join("node_modules").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_mode_does_not_pollute_source_when_container_overwrites_metadata() {
        // PoC 2: editing the container's metadata file must not change the
        // source tree, because install mode copies metadata to a per-
        // container real file.
        let root = sandbox("no-pollution");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("package.json"), r#"{"version":"1.0.0"}"#).unwrap();
        let destination = root.join("destination");
        install_plugin_to_container_mode(&source, &destination).unwrap();
        fs::write(destination.join("package.json"), r#"{"version":"2.0.0"}"#).unwrap();
        let source_after = fs::read_to_string(source.join("package.json")).unwrap();
        assert_eq!(source_after, r#"{"version":"1.0.0"}"#);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_mode_inode_is_shared_across_multiple_destinations() {
        // PoC 3: two containers pointing at the same repository entry must
        // share the inode of any code file; metadata stays per-container.
        let root = sandbox("shared-inode");
        let source = root.join("source");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("package.json"), "{}").unwrap();
        fs::write(source.join("src/lib.ts"), "export const x = 1;").unwrap();

        let dest_a = root.join("container-a");
        let dest_b = root.join("container-b");
        install_plugin_to_container_mode(&source, &dest_a).unwrap();
        install_plugin_to_container_mode(&source, &dest_b).unwrap();

        assert_same_shared_file(&dest_a.join("src/lib.ts"), &dest_b.join("src/lib.ts"));

        // But each container holds its own copy of the metadata file.
        let meta_a = fs::read_to_string(dest_a.join("package.json")).unwrap();
        let meta_b = fs::read_to_string(dest_b.join("package.json")).unwrap();
        assert_eq!(meta_a, meta_b);
        // Each container holds its own copy of the metadata file: writing
        // one must not change the other.
        fs::write(dest_a.join("package.json"), r#"{"version":"2.0.0"}"#).unwrap();
        assert_eq!(
            fs::read_to_string(dest_b.join("package.json")).unwrap(),
            "{}",
            "metadata must be per-container copies, not links"
        );
        let _ = fs::remove_dir_all(root);
    }
}
