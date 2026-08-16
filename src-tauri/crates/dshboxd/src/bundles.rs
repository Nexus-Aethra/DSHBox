//! Plugin/skill installation and bundle management for daemon-run tasks.
//! Mirrors the desktop's `bundles.rs` (`install_container_plugin`,
//! `install_container_skill`, bundle create/delete/export/import).

use crate::extensions::repository_metadata;
use crate::toolchains::{command_for_toolchain, resolve_toolchain, wait_for_process};
use box_containers::DshContainer;
use box_dsh_versions::version_directory as dsh_version_directory;
use box_extensions::transfer::{
    append_plugin_archive, copy_extension_source, extract_extension_tarball,
    install_plugin_to_container_mode,
};
use box_extensions::{
    detect_extension_kind, directory_size, extension_digest, read_bundles, repository_root,
    scan_repository, write_bundles, write_extension_record, write_repository_index, BundleEntry,
    ExtensionBundle, ExtensionKind, ExtensionRecord, RepositoryExtension,
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

pub(crate) fn skill_name(path: &Path) -> Result<String, String> {
    let content =
        fs::read_to_string(path).map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    let name = content
        .lines()
        .find_map(|line| line.strip_prefix("name:").map(str::trim))
        .ok_or("skill frontmatter has no name")?;
    Ok(name.trim_matches(['\'', '"']).to_owned())
}

pub(crate) fn install_container_skill(
    container: &DshContainer,
    source_kind: &str,
    source: &str,
    extracted: PathBuf,
    task: &TaskContext,
) -> Result<(), String> {
    let name = skill_name(&extracted.join("SKILL.md"))?;
    if !is_safe_identifier(&name) {
        return Err(
            "skill name must use letters, numbers, dots, dashes, or underscores".to_owned(),
        );
    }
    let destination = PathBuf::from(&container.directory)
        .join("profile/skills")
        .join(&name);
    if destination.exists() {
        return Err(format!("skill already exists: {name}"));
    }
    task.update("Installing container skill", 65);
    fs::create_dir_all(destination.parent().ok_or("skill destination has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::rename(&extracted, &destination)
        .map_err(|error| format!("cannot install skill: {error}"))?;
    write_extension_record(
        container,
        ExtensionRecord {
            kind: ExtensionKind::Skill,
            name,
            source_kind: source_kind.to_owned(),
            source: source.to_owned(),
            profile: None,
            path: destination.to_string_lossy().into_owned(),
            installed_at: now_seconds(),
            repository_id: if source_kind == "repository" {
                Some(source.to_owned())
            } else {
                None
            },
            content_digest: extension_digest(&destination).ok(),
        },
    )
}

pub(crate) fn install_container_plugin(
    container: &DshContainer,
    profile: &str,
    source_kind: &str,
    source: &str,
    extracted: PathBuf,
    task: &TaskContext,
) -> Result<(), String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(extracted.join("package.json"))
            .map_err(|error| format!("cannot read plugin package.json: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin package.json: {error}"))?;
    let name = manifest["name"]
        .as_str()
        .ok_or("plugin package.json has no name")?
        .to_owned();
    let source_directory = PathBuf::from(&container.directory)
        .join("extensions/plugins")
        .join(if source_kind == "repository" {
            source
        } else {
            &task.task_id
        })
        .join("source");
    if source_directory.exists() {
        return Err(format!(
            "plugin is already installed from this repository entry: {name}"
        ));
    }
    fs::create_dir_all(source_directory.parent().ok_or("plugin destination has no parent")?)
        .map_err(|error| error.to_string())?;
    if source_kind == "repository" {
        // Repository-sourced plugins are materialised as a hybrid view: the
        // plugin root (metadata files) becomes a real per-container
        // directory while code subtrees (src/lib/dist/...) become links
        // back into the shared repository entry. Containers therefore share
        // code inodes while still owning everything they might edit.
        // `extracted` is the repository entry's source path in this mode.
        install_plugin_to_container_mode(&extracted, &source_directory)?;
    } else {
        // External sources (github/tarball/user directory) are staged per
        // task; move the whole tree so the container owns a full copy.
        fs::rename(&extracted, &source_directory)
            .map_err(|error| format!("cannot store plugin source: {error}"))?;
    }
    task.update("Installing DSH plugin", 60);
    task.log(&format!("adding plugin {name} to profile {profile}"));
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let dsh_source = dsh_version_directory(&root, &container.version);
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.task_id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let mut child = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            dsh_source.to_string_lossy().as_ref(),
            "dsh",
            "plugin",
            "--profile",
            profile,
            "add",
            source_directory.to_string_lossy().as_ref(),
        ])
        .env(
            "DSH_HOME",
            PathBuf::from(&container.directory).join("profile"),
        )
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
    write_extension_record(
        container,
        ExtensionRecord {
            kind: ExtensionKind::Plugin,
            name,
            source_kind: source_kind.to_owned(),
            source: source.to_owned(),
            profile: Some(profile.to_owned()),
            path: source_directory.to_string_lossy().into_owned(),
            installed_at: now_seconds(),
            repository_id: if source_kind == "repository" {
                Some(source.to_owned())
            } else {
                None
            },
            content_digest: extension_digest(&source_directory).ok(),
        },
    )
}

fn is_github_source(source: &str) -> bool {
    source.trim_start().starts_with("https://github.com/")
}

/// Create a bundle from repository entry ids (docker-style: name + picks).
pub(crate) fn create_extension_bundle(
    name: &str,
    repository_ids: &[String],
) -> Result<ExtensionBundle, String> {
    let name = name.trim();
    if name.is_empty() || repository_ids.is_empty() {
        return Err("bundle needs a name and at least one extension".to_owned());
    }
    if repository_ids.len() > 32 {
        return Err("bundle supports at most 32 extensions".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let entries = scan_repository(Path::new(&root));
    let mut picked = Vec::new();
    for id in repository_ids {
        let entry = entries
            .iter()
            .find(|entry| entry.id == *id)
            .ok_or("repository extension not found")?;
        if let Some(diagnostic) = &entry.diagnostic {
            return Err(format!(
                "extension {} is not usable: {diagnostic}",
                entry.name
            ));
        }
        picked.push(BundleEntry {
            repository_id: entry.id.clone(),
            kind: entry.kind.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            source: entry.source.clone(),
            size: directory_size(Path::new(&entry.source_path)),
            diagnostic: None,
        });
    }
    picked.sort_by(|left, right| left.name.cmp(&right.name));
    let mut bundles = read_bundles(Path::new(&root));
    let bundle = ExtensionBundle {
        id: format!("bundle-{}", now_seconds()),
        name: name.to_owned(),
        entries: picked,
        created_at: now_seconds(),
    };
    bundles.push(bundle.clone());
    write_bundles(Path::new(&root), &bundles)?;
    Ok(bundle)
}

pub(crate) fn delete_extension_bundle(id: &str) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut bundles = read_bundles(Path::new(&root));
    let before = bundles.len();
    bundles.retain(|bundle| bundle.id != id);
    if bundles.len() == before {
        return Err("bundle not found".to_owned());
    }
    write_bundles(Path::new(&root), &bundles)
}

/// Exports a named bundle as a tarball whose first entry is a manifest list
/// describing every member. Quick exports keep GitHub-sourced entries as
/// URLs in the manifest instead of embedding their content; full exports
/// embed everything.
pub(crate) fn export_extension_bundle(
    id: &str,
    destination: &str,
    mode: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let bundle = read_bundles(Path::new(&root))
        .into_iter()
        .find(|bundle| bundle.id == id)
        .ok_or("bundle not found")?;
    let quick = mode == "quick";
    let destination = PathBuf::from(destination);
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("bundle export has no parent directory")?,
    )
    .map_err(|error| error.to_string())?;
    task.update("Packaging extension bundle", 30);
    task.log(&format!(
        "exporting bundle {} ({}) to {}",
        bundle.name,
        if quick { "quick" } else { "full" },
        destination.display()
    ));
    let repository = scan_repository(Path::new(&root));
    let source_path = |repository_id: &str| {
        repository
            .iter()
            .find(|entry| entry.id == repository_id)
            .map(|entry| entry.source_path.clone())
    };
    let manifest_entries = bundle
        .entries
        .iter()
        .map(|entry| {
            let github = entry
                .source
                .as_deref()
                .map(is_github_source)
                .unwrap_or(false);
            serde_json::json!({
                "type": match entry.kind {
                    ExtensionKind::Plugin => "plugin",
                    ExtensionKind::Skill => "skill",
                },
                "name": entry.name,
                "version": entry.version,
                "size": entry.size,
                "source": entry.source,
                "embedded": !(quick && github),
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "format": "dsh-bundle",
        "version": 1,
        "name": bundle.name,
        "mode": mode,
        "exportedAt": now_seconds(),
        "entries": manifest_entries,
    });
    let output = fs::File::create(&destination)
        .map_err(|error| format!("cannot create bundle tarball: {error}"))?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "manifest.json", manifest_bytes.as_slice())
        .map_err(|error| format!("cannot append bundle manifest: {error}"))?;
    for entry in &bundle.entries {
        task.check_cancelled()?;
        let Some(source_path) = source_path(&entry.repository_id) else {
            task.log(&format!("skipping {}: repository source is gone", entry.name));
            continue;
        };
        let source = Path::new(&source_path);
        if !source.is_dir() {
            task.log(&format!("skipping {}: source directory is missing", entry.name));
            continue;
        }
        let github = entry
            .source
            .as_deref()
            .map(is_github_source)
            .unwrap_or(false);
        let target = Path::new(match entry.kind {
            ExtensionKind::Plugin => "plugins",
            ExtensionKind::Skill => "skills",
        })
        .join(&entry.name);
        if quick && github {
            task.log(&format!("quick: {} kept as URL only", entry.name));
            continue;
        }
        task.log(&format!("packing {} ({})", entry.name, target.display()));
        archive
            .append_dir(&target, source)
            .map_err(|error| error.to_string())?;
        append_plugin_archive(&mut archive, source, &target)?;
    }
    archive.finish().map_err(|error| error.to_string())?;
    task.check_cancelled()?;
    task.update("Bundle exported", 95);
    Ok(())
}

/// Imports a bundle archive into the extension repository: reads the
/// manifest, materialises every entry (embedded content or a GitHub clone),
/// resolves name clashes per the chosen conflict mode, and registers the
/// imported set as a new bundle.
pub(crate) fn import_extension_bundle(
    archive: &str,
    conflict: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let archive = PathBuf::from(archive);
    task.update("Reading bundle manifest", 15);
    task.log(&format!("importing bundle {}", archive.display()));
    let staging = repository_root(Path::new(&root))
        .join("staging")
        .join(&task.task_id);
    let _ = fs::remove_dir_all(&staging);
    let extracted_root = staging.join("extracted");
    fs::create_dir_all(&extracted_root).map_err(|error| error.to_string())?;
    extract_extension_tarball(&archive, &extracted_root)?;
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(extracted_root.join("manifest.json"))
            .map_err(|_| "bundle archive has no manifest.json".to_owned())?,
    )
    .map_err(|error| format!("cannot parse bundle manifest: {error}"))?;
    if manifest["format"].as_str() != Some("dsh-bundle") {
        return Err("not a DSH Box bundle archive".to_owned());
    }
    let bundle_name = manifest["name"].as_str().unwrap_or("imported-bundle").to_owned();
    let manifest_entries = manifest["entries"]
        .as_array()
        .cloned()
        .ok_or("bundle manifest has no entries")?;
    let mut repository = scan_repository(Path::new(&root));
    task.update("Importing bundle extensions", 40);
    for (index, entry) in manifest_entries.into_iter().enumerate() {
        task.check_cancelled()?;
        let name = entry["name"]
            .as_str()
            .ok_or("bundle entry has no name")?
            .to_owned();
        let kind = if entry["type"].as_str() == Some("skill") {
            ExtensionKind::Skill
        } else {
            ExtensionKind::Plugin
        };
        let source = entry["source"].as_str().map(str::to_owned);
        let folder = if kind == ExtensionKind::Plugin {
            "plugins"
        } else {
            "skills"
        };
        let mut extracted_dir = extracted_root.join(folder).join(&name);
        if !extracted_dir.is_dir() {
            // Quick entries carry no content; fetch from GitHub when possible.
            let Some(url) = source.as_deref().filter(|url| is_github_source(url)) else {
                task.log(&format!(
                    "skipping {name}: no embedded content and no GitHub source"
                ));
                continue;
            };
            task.log(&format!("fetching {name} from {url}"));
            extracted_dir = staging.join(format!("fetched-{index}"));
            let config = read_config()?;
            let cancelled = task.clone();
            shallow_clone_with_cancel(
                &mirror_url(url, config.github_mirror.as_deref()),
                &extracted_dir,
                None,
                move || cancelled.cancelled(),
            )?;
        }
        if !extracted_dir.is_dir() {
            task.log(&format!("skipping {name}: content is missing"));
            continue;
        }
        let detected = detect_extension_kind(&extracted_dir)?;
        if detected != kind {
            task.log(&format!(
                "skipping {name}: manifest says {kind:?} but content is {detected:?}"
            ));
            continue;
        }
        let (real_name, version, _) = repository_metadata(&kind, &extracted_dir)?;
        let clashes = |candidate: &str| {
            repository
                .iter()
                .any(|entry| entry.kind == kind && entry.name == candidate)
        };
        let final_name = if clashes(&real_name) {
            if conflict == "overwrite" {
                let stale = repository
                    .iter()
                    .filter(|entry| entry.kind == kind && entry.name == real_name)
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>();
                for id in &stale {
                    if let Some(old) = repository.iter().find(|entry| entry.id == *id) {
                        let _ = fs::remove_dir_all(
                            PathBuf::from(&old.source_path)
                                .parent()
                                .unwrap_or(Path::new("")),
                        );
                    }
                }
                repository.retain(|entry| !stale.contains(&entry.id));
                real_name.clone()
            } else {
                let mut n = 2;
                while clashes(&format!("{real_name} ({n})")) {
                    n += 1;
                }
                format!("{real_name} ({n})")
            }
        } else {
            real_name.clone()
        };
        let repo_id = format!("{}-{}", task.task_id, index);
        let repo_dir = repository_root(Path::new(&root))
            .join(folder)
            .join(&repo_id)
            .join("source");
        if repo_dir.exists() {
            return Err(format!("repository entry {repo_id} already exists"));
        }
        fs::create_dir_all(repo_dir.parent().ok_or("repository entry has no parent")?)
            .map_err(|error| error.to_string())?;
        fs::rename(&extracted_dir, &repo_dir)
            .map_err(|error| format!("cannot store repository source: {error}"))?;
        task.log(&format!("imported {kind:?} {final_name}"));
        repository.push(RepositoryExtension {
            id: repo_id,
            kind,
            name: final_name,
            version,
            description: None,
            content_digest: extension_digest(&repo_dir)?,
            source_path: repo_dir.to_string_lossy().into_owned(),
            imported_at: now_seconds(),
            diagnostic: None,
            source,
        });
    }
    repository.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.id.cmp(&right.id))
    });
    write_repository_index(Path::new(&root), &repository)?;
    // Register the imported set as a bundle so it shows in the bundle list.
    let prefix = format!("{}-", task.task_id);
    let bundle_entries = repository
        .iter()
        .filter(|entry| entry.id.starts_with(&prefix))
        .map(|entry| BundleEntry {
            repository_id: entry.id.clone(),
            kind: entry.kind.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            source: entry.source.clone(),
            size: directory_size(Path::new(&entry.source_path)),
            diagnostic: None,
        })
        .collect::<Vec<_>>();
    if !bundle_entries.is_empty() {
        let mut bundles = read_bundles(Path::new(&root));
        bundles.push(ExtensionBundle {
            id: format!("bundle-{}", now_seconds()),
            name: bundle_name,
            entries: bundle_entries,
            created_at: now_seconds(),
        });
        write_bundles(Path::new(&root), &bundles)?;
    }
    let _ = fs::remove_dir_all(staging);
    task.update("Bundle imported", 95);
    Ok(())
}

/// Install every entry of a bundle into one container, sorting plugins into
/// the profile and skills into the container skill root automatically.
/// Mirrors the desktop's `install_container_bundle` without Tauri deps.
pub(crate) fn install_container_bundle(
    container_id: &str,
    profile: &str,
    bundle_id: &str,
    conflict: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(container_id)
        .ok_or("container not found")?;
    let bundle = read_bundles(Path::new(&root))
        .into_iter()
        .find(|bundle| bundle.id == bundle_id)
        .ok_or("bundle not found")?;
    let repository = scan_repository(Path::new(&root));
    task.update("Installing bundle into container", 20);
    task.log(&format!(
        "installing bundle {} into container {}",
        bundle.name, container.name
    ));
    let staging = PathBuf::from(&container.directory)
        .join("extensions/staging")
        .join(&task.task_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    for (index, entry) in bundle.entries.iter().enumerate() {
        task.check_cancelled()?;
        let repo_entry = repository
            .iter()
            .find(|candidate| candidate.id == entry.repository_id)
            .ok_or("bundle references a missing repository entry")?;
        let source_dir = PathBuf::from(&repo_entry.source_path);
        match entry.kind {
            ExtensionKind::Plugin => {
                // Repository-sourced plugins link their code subtrees into
                // the container (sharing inodes); no staging copy needed.
                let installed = PathBuf::from(&container.directory)
                    .join("extensions/plugins")
                    .join(&repo_entry.name)
                    .join("source");
                if installed.exists() {
                    if conflict == "keep" {
                        task.log(&format!(
                            "keep: plugin {} already installed, skipping",
                            repo_entry.name
                        ));
                        continue;
                    }
                    task.log(&format!(
                        "overwrite: replacing plugin {}",
                        repo_entry.name
                    ));
                    let _ = fs::remove_dir_all(
                        installed.parent().ok_or("plugin install has no parent")?,
                    );
                }
                install_container_plugin(
                    &container,
                    profile,
                    "repository",
                    &repo_entry.name,
                    source_dir,
                    task,
                )?;
            }
            ExtensionKind::Skill => {
                // Skills may be edited per container; copy them into a
                // staging dir first so install_container_skill can move
                // them into place.
                let staged = staging.join(format!("entry-{index}"));
                copy_extension_source(&source_dir, &staged)?;
                let name = skill_name(&staged.join("SKILL.md"))?;
                let destination = PathBuf::from(&container.directory)
                    .join("profile/skills")
                    .join(&name);
                if destination.exists() {
                    if conflict == "keep" {
                        task.log(&format!(
                            "keep: skill {name} already installed, skipping"
                        ));
                        continue;
                    }
                    task.log(&format!("overwrite: replacing skill {name}"));
                    let _ = fs::remove_dir_all(&destination);
                }
                install_container_skill(
                    &container,
                    "repository",
                    &repo_entry.name,
                    staged,
                    task,
                )?;
            }
        }
    }
    let _ = fs::remove_dir_all(staging);
    task.update("Bundle installed into container", 95);
    Ok(())
}
