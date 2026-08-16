//! Repository extension management for the daemon: copy into containers,
//! import/export entries, remove entries, and direct `dsh plugin add`.
//! Mirrors the desktop's `extensions.rs` paths without Tauri deps.

use crate::bundles::{install_container_plugin, install_container_skill};
use crate::toolchains::{command_for_toolchain, resolve_toolchain, wait_for_process};
use box_dsh_versions::version_directory as dsh_version_directory;
use box_extensions::{
    detect_extension_kind, read_bundles, remove_plugin_record, repository_root, scan_repository,
    write_bundles, write_repository_index, ExtensionKind, RepositoryExtension,
};
use box_extensions::transfer::{
    append_plugin_archive, archive_content_root, copy_extension_source, extract_extension_tarball,
};
use box_foundation::{is_safe_identifier, mirror_url, now_seconds, read_config};
use box_runtime::shallow_clone_with_cancel;
use box_scheduler::TaskContext;
use flate2::{write::GzEncoder, Compression};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

pub(crate) fn repository_metadata(
    kind: &ExtensionKind,
    source: &Path,
) -> Result<(String, Option<String>, Option<String>), String> {
    match kind {
        ExtensionKind::Skill => {
            let content = fs::read_to_string(source.join("SKILL.md"))
                .map_err(|error| error.to_string())?;
            let field = |key: &str| {
                content
                    .lines()
                    .find_map(|line| line.strip_prefix(key).map(str::trim))
                    .map(|value| value.trim_matches(['\'', '"']).to_owned())
            };
            Ok((
                field("name:").ok_or("skill frontmatter has no name")?,
                None,
                field("description:"),
            ))
        }
        ExtensionKind::Plugin => {
            let value: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(source.join("package.json")).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            Ok((
                value["name"]
                    .as_str()
                    .ok_or("plugin package.json has no name")?
                    .to_owned(),
                value["version"].as_str().map(str::to_owned),
                value["description"].as_str().map(str::to_owned),
            ))
        }
    }
}

/// Link a repository entry into a container profile (plugin) or skill root.
/// Plugins are materialised as a hybrid view (real root directory + code
/// subdirectory links sharing inodes with the repository); skills are
/// copied so every container owns an editable copy.
pub(crate) fn link_repository_extension(
    container_id: &str,
    profile: Option<&str>,
    repository_id: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let entry = scan_repository(Path::new(&root))
        .into_iter()
        .find(|entry| entry.id == repository_id)
        .ok_or("repository extension not found")?;
    if entry.diagnostic.is_some() {
        return Err("repository extension is invalid".to_owned());
    }
    let container = box_containers::scan_containers(&root)?
        .remove(container_id)
        .ok_or("container not found")?;
    task.update("Linking repository extension", 25);
    match entry.kind {
        ExtensionKind::Plugin => {
            let profile = profile.ok_or("plugin installation requires a profile")?;
            if !std::path::PathBuf::from(&container.directory)
                .join("profile/profiles")
                .join(profile)
                .join("package.json")
                .is_file()
            {
                return Err(format!("profile not found: {profile}"));
            }
            // Pass the repository source path directly: the installer
            // links code subtrees into the container instead of copying
            // them, so all containers share the same plugin inodes.
            let source_path = PathBuf::from(&entry.source_path);
            install_container_plugin(&container, profile, "repository", &entry.id, source_path, task)?;
            // The container now links this shared entry: record ownership
            // so `plugin prune` never removes in-use plugins.
            box_extensions::increment_reference(Path::new(&root), &entry.id)?;
        }
        ExtensionKind::Skill => {
            // Skills stay per-container copies: install_container_skill
            // moves the source into place, so stage a copy first.
            let staging = PathBuf::from(&container.directory)
                .join("extensions/staging")
                .join(&task.task_id)
                .join("source");
            fs::create_dir_all(staging.parent().ok_or("extension staging has no parent")?)
                .map_err(|error| error.to_string())?;
            copy_extension_source(Path::new(&entry.source_path), &staging)?;
            install_container_skill(&container, "repository", &entry.id, staging, task)?
        }
    }
    task.update("Container extension installed", 95);
    Ok(())
}

/// Import a directory into the repository index (used by build scripts
/// when a GitHub/tarball/local source is fetched).
pub(crate) fn import_into_repository(
    task: &TaskContext,
    source: &Path,
) -> Result<RepositoryExtension, String> {
    let kind = detect_extension_kind(source)
        .map_err(|error| format!("extension validation failed: {error}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let (name, version, description) =
        repository_metadata(&kind, source).map_err(|error| format!("repository metadata: {error}"))?;
    let entry_id = format!("img-{}", task.task_id);
    let destination = repository_root(Path::new(&root))
        .join(match kind {
            ExtensionKind::Plugin => "plugins",
            ExtensionKind::Skill => "skills",
        })
        .join(&entry_id)
        .join("source");
    if destination.exists() {
        return Err(format!("repository entry already exists: {entry_id}"));
    }
    fs::create_dir_all(destination.parent().ok_or("destination has no parent")?)
        .map_err(|error| error.to_string())?;
    copy_extension_source(source, &destination)?;
    let digest = box_extensions::extension_digest(&destination)?;
    let mut entries = scan_repository(Path::new(&root));
    entries.push(RepositoryExtension {
        id: entry_id.clone(),
        kind,
        name,
        version,
        description,
        content_digest: digest,
        source_path: destination.to_string_lossy().into_owned(),
        imported_at: now_seconds(),
        diagnostic: None,
        source: Some(source.to_string_lossy().into_owned()),
    });
    write_repository_index(Path::new(&root), &entries)?;
    Ok(entries
        .into_iter()
        .find(|entry| entry.id == entry_id)
        .expect("entry we just pushed"))
}

/// Export a repository entry as a `.tar.gz` archive.
pub(crate) fn export_repository_extension(
    repository_id: &str,
    destination: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let entry = scan_repository(Path::new(&root))
        .into_iter()
        .find(|entry| entry.id == repository_id)
        .ok_or("repository extension not found")?;
    task.update("Packaging extension tarball", 30);
    box_extensions::transfer::export_extension_directory(
        Path::new(&entry.source_path),
        Path::new(destination),
    )?;
    task.check_cancelled()?;
    task.update("Extension tarball exported", 95);
    Ok(())
}

/// Remove a repository entry and every bundle entry that referenced it, so
/// bundles never keep dangling references. Entries still used by at least
/// one container (reference count > 0) are rejected: removing them would
/// leave dangling links inside running containers.
pub(crate) fn remove_repository_extension(id: &str) -> Result<(), String> {
    if !box_foundation::is_safe_identifier(id) {
        return Err("invalid repository extension id".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut entries = scan_repository(Path::new(&root));
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
        .ok_or("repository extension not found")?;
    let used_by = box_extensions::reference_count(Path::new(&root), id);
    if used_by > 0 {
        return Err(format!(
            "repository extension `{}` is used by {used_by} container(s); remove them first or run `dshbox plugin prune`",
            entry.name
        ));
    }
    fs::remove_dir_all(PathBuf::from(&entry.source_path).parent().ok_or(
        "repository source has no parent",
    )?)
    .map_err(|error| error.to_string())?;
    entries.retain(|entry| entry.id != id);
    write_repository_index(Path::new(&root), &entries)?;
    let mut bundles = read_bundles(Path::new(&root));
    for bundle in &mut bundles {
        bundle.entries.retain(|entry| entry.repository_id != id);
    }
    write_bundles(Path::new(&root), &bundles)?;
    Ok(())
}

/// Remove every repository entry whose reference count is zero. Returns
/// the removed ids; entries still in use are kept.
pub(crate) fn prune_unused_repository_extensions() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut removed = Vec::new();
    for id in box_extensions::unused_repository_ids(Path::new(&root)) {
        if let Err(error) = remove_repository_extension(&id) {
            return Err(format!("cannot prune {id}: {error}"));
        }
        removed.push(id);
    }
    Ok(removed)
}

/// Install a package spec directly into a container profile via
/// `dsh plugin add` (mirrors the CLI's `plugin install` action).
pub(crate) fn container_plugin_add(
    container_id: &str,
    profile: &str,
    spec: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(container_id)
        .ok_or("container not found")?;
    let source = dsh_version_directory(&root, &container.version);
    let pnpm = resolve_toolchain("pnpm")?;
    task.update("Installing DSH plugin", 60);
    task.log(&format!("adding plugin {spec} to profile {profile}"));
    let task_record = task.manager.task(&task.task_id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let mut child = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            source.to_string_lossy().as_ref(),
            "dsh",
            "plugin",
            "--profile",
            profile,
            "add",
            spec,
        ])
        .env("DSH_HOME", PathBuf::from(&container.directory).join("profile"))
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| format!("cannot start plugin install: {error}"))?;
    let status = wait_for_process(&mut child, Some(task), "installing plugin")?;
    if !status.success() {
        return Err(format!("dsh plugin add exited with {status}"));
    }
    task.update("Plugin installed", 95);
    Ok(())
}

/// Copy a container-safe package name check (mirrors the desktop's).
pub(crate) fn is_safe_package_name(name: &str) -> bool {
    !name.is_empty() && !name.contains("..") && name.split('/').all(is_safe_identifier)
}

/// Install an extension (GitHub URL, repository path, or tarball) into a
/// container profile directly. Mirrors the desktop's
/// `install_container_extension` without Tauri dependencies.
pub(crate) fn install_container_extension(
    container_id: &str,
    profile: &str,
    source: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(container_id)
        .ok_or("container not found")?;
    let profile_dir = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(profile);
    if !profile_dir.join("package.json").is_file() {
        return Err(format!("profile not found: {profile}"));
    }
    let source = source.trim();
    let source_kind = if source.starts_with("https://github.com/") {
        "github"
    } else if Path::new(source).is_dir() {
        // A user-provided directory is copied into a per-container staging
        // dir and installed as a plain copy ("local"): it is not a shared
        // repository entry, so it must never be linked.
        "local"
    } else {
        "tarball"
    };
    let staging = PathBuf::from(&container.directory)
        .join("extensions/staging")
        .join(&task.task_id);
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    task.update("Importing extension source", 15);
    let extracted = if source_kind == "github" {
        let destination = staging.join("source");
        let config = read_config()?;
        let target = mirror_url(source, config.github_mirror.as_deref());
        task.log(&format!("cloning GitHub repository {source}"));
        let cancelled = task.clone();
        shallow_clone_with_cancel(&target, &destination, None, move || cancelled.cancelled())?;
        destination
    } else if source_kind == "local" {
        let destination = staging.join("source");
        task.log(&format!("copying plugin from local directory {source}"));
        copy_extension_source(Path::new(source), &destination)?;
        destination
    } else {
        let archive = PathBuf::from(source);
        if !archive.is_file() {
            return Err("tarball source must be an existing local file".to_owned());
        }
        task.log(&format!("extracting tarball {}", archive.display()));
        let destination = staging.join("source");
        fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
        extract_extension_tarball(&archive, &destination)?;
        archive_content_root(&destination)?
    };
    task.check_cancelled()?;
    task.update("Detecting extension type", 40);
    let kind = detect_extension_kind(&extracted)?;
    match kind {
        ExtensionKind::Skill => {
            install_container_skill(&container, source_kind, source, extracted, task)
        }
        ExtensionKind::Plugin => install_container_plugin(
            &container,
            profile,
            source_kind,
            source,
            extracted,
            task,
        ),
    }?;
    let _ = fs::remove_dir_all(staging);
    task.update("Refreshing container extensions", 95);
    Ok(())
}

/// Copy a workspace subdirectory of a container into the extension
/// repository. Mirrors the desktop's `import_workspace_extension`.
pub(crate) fn import_workspace_extension(
    container_id: &str,
    relative_path: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(container_id)
        .ok_or("container not found")?;
    let workspace = PathBuf::from(container.directory)
        .join("workspace")
        .canonicalize()
        .map_err(|error| format!("cannot access container workspace: {error}"))?;
    let source = workspace
        .join(relative_path)
        .canonicalize()
        .map_err(|error| format!("workspace extension no longer exists: {error}"))?;
    if !source.starts_with(&workspace) || !source.is_dir() {
        return Err("workspace extension escaped the container workspace".to_owned());
    }
    task.log(&format!("copying workspace extension {}", source.display()));
    import_into_repository(task, &source).map(|_| ())
}

/// Export a container-managed plugin directory as a `.tar.gz` archive.
/// Mirrors the desktop's `export_repository_plugin` without Tauri deps.
pub(crate) fn export_repository_plugin(
    source_container_id: &str,
    source_path: &str,
    destination: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let instance_root = PathBuf::from(&root)
        .join("instances")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let source = PathBuf::from(source_path)
        .canonicalize()
        .map_err(|error| format!("cannot find plugin source: {error}"))?;
    if !source.starts_with(&instance_root) || !source.join("package.json").is_file() {
        return Err("plugin source is not a DSH Box managed plugin".to_owned());
    }
    if !is_safe_identifier(source_container_id) {
        return Err("invalid container id".to_owned());
    }
    let destination = PathBuf::from(destination);
    if destination.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("plugin export destination must end in .tar.gz".to_owned());
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("plugin export has no parent directory")?,
    )
    .map_err(|error| error.to_string())?;
    task.update("Packaging plugin tarball", 30);
    task.log(&format!(
        "exporting {} to {}",
        source.display(),
        destination.display()
    ));
    let output = fs::File::create(&destination)
        .map_err(|error| format!("cannot create plugin tarball: {error}"))?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_plugin_archive(&mut archive, &source, Path::new("extension"))?;
    archive.finish().map_err(|error| error.to_string())?;
    task.check_cancelled()?;
    task.update("Plugin tarball exported", 95);
    Ok(())
}

/// Disable a plugin in a container profile and remove its records.
/// Mirrors the desktop's `remove_repository_plugin` without Tauri deps.
pub(crate) fn remove_repository_plugin(
    id: &str,
    profile: &str,
    name: &str,
) -> Result<(), String> {
    if !is_safe_identifier(id) || !is_safe_identifier(profile) || !is_safe_package_name(name) {
        return Err("invalid plugin removal request".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(id)
        .ok_or("container not found")?;
    let profile_directory = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(profile);
    let manifest_path = profile_directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let bundles = manifest
        .pointer_mut("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("profile has no plugin bundle list")?;
    let original = bundles.len();
    bundles.retain(|item| item.as_str() != Some(name));
    if bundles.len() == original {
        return Err("plugin is not enabled in this profile".to_owned());
    }
    if let Some(dependencies) = manifest
        .get_mut("dependencies")
        .and_then(serde_json::Value::as_object_mut)
    {
        dependencies.remove(name);
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let link = profile_directory.join("node_modules").join(name);
    if link.exists() {
        fs::remove_dir_all(&link).map_err(|error| error.to_string())?;
    }
    remove_plugin_record(&container, profile, name)?;
    Ok(())
}

/// List the bundles a container profile enables (`dshbox plugin ls <id>`).
pub(crate) fn container_list_plugins(id: &str, profile: &str) -> Result<Vec<String>, String> {
    if !box_foundation::is_safe_identifier(id) || !box_foundation::is_safe_identifier(profile) {
        return Err("invalid container or profile name".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(id)
        .ok_or("container not found")?;
    let manifest = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(profile)
        .join("package.json");
    let value: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest)
            .map_err(|_| format!("profile not found: {}", manifest.display()))?,
    )
    .map_err(|error| error.to_string())?;
    Ok(value
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect())
}
