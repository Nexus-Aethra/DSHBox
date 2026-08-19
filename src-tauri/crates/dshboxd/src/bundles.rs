//! Plugin/skill installation and bundle management for daemon-run tasks.
//! Mirrors the desktop's `bundles.rs` (`install_container_plugin`,
//! `install_container_skill`, bundle create/delete/export/import).

use crate::extensions::repository_metadata;
use crate::toolchains::{pnpm_policy, resolve_toolchain, run_logged, TaskCancel};
use box_containers::DshContainer;
use box_extensions::transfer::{
    append_plugin_archive, copy_extension_source, extract_extension_tarball,
};
use box_extensions::{
    detect_extension_kind, directory_size, extension_digest, read_bundles, repository_root,
    scan_repository, write_bundles, write_extension_record, write_repository_index, BundleEntry,
    ExtensionBundle, ExtensionKind, ExtensionRecord, RepositoryExtension,
};
use box_foundation::{is_safe_identifier, mirror_url, now_seconds, read_config};
use box_runtime::process::{ExecutionKind, ProcessSpec};
use box_runtime::shallow_clone_with_cancel;
use box_scheduler::TaskContext;
use flate2::{write::GzEncoder, Compression};
use std::time::Duration;
use std::{
    fs,
    path::{Path, PathBuf},
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

    let profile_dir = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(profile);
    if !profile_dir.join("package.json").is_file() {
        return Err(format!("profile not found: {profile}"));
    }

    // Determine the persistent source path for this plugin.
    // Repository entries already live in the shared runtime directory;
    // use them directly so containers share the same source.
    // External sources live in a staging dir that gets cleaned up; persist
    // them into a per-container directory first.
    let plugin_source = if source_kind == "repository" {
        if box_extensions::read_extension_records(container)
            .iter()
            .any(|r| r.name == name && r.profile.as_deref() == Some(profile))
        {
            return Err(format!(
                "plugin {name} is already installed in profile {profile}"
            ));
        }
        extracted
    } else {
        let source_directory = PathBuf::from(&container.directory)
            .join("extensions/plugins")
            .join(&task.task_id)
            .join("source");
        if source_directory.exists() {
            return Err(format!("plugin is already installed: {name}"));
        }
        fs::create_dir_all(
            source_directory
                .parent()
                .ok_or("plugin destination has no parent")?,
        )
        .map_err(|error| error.to_string())?;
        fs::rename(&extracted, &source_directory)
            .map_err(|error| format!("cannot store plugin source: {error}"))?;
        source_directory
    };

    task.update("Installing DSH plugin", 60);
    task.log(&format!("adding plugin {name} to profile {profile}"));
    // Register the plugin directly as a pnpm workspace member instead of
    // going through `dsh plugin add`. The DSH CLI's `plugin add` internally
    // runs `pnpm add <link:path>` with cwd=profile_dir, which triggers
    // ERR_PNPM_ADDING_TO_ROOT in pnpm 11 (refuses to add deps to workspace
    // root). Direct registration adds the plugin as `workspace:*` and runs
    // `pnpm install` which has no such restriction.
    register_plugin_directly(&profile_dir, &plugin_source, &name, task)?;
    write_extension_record(
        container,
        ExtensionRecord {
            kind: ExtensionKind::Plugin,
            name,
            source_kind: source_kind.to_owned(),
            source: source.to_owned(),
            profile: Some(profile.to_owned()),
            path: plugin_source.to_string_lossy().into_owned(),
            installed_at: now_seconds(),
            repository_id: if source_kind == "repository" {
                Some(source.to_owned())
            } else {
                None
            },
            content_digest: extension_digest(&plugin_source).ok(),
        },
    )
}

fn is_github_source(source: &str) -> bool {
    source.trim_start().starts_with("https://github.com/")
}

/// Register a plugin as a pnpm workspace member directly, bypassing
/// `dsh plugin add`. Steps:
/// 1. Update profile package.json: add `workspace:*` dependency + DSH bundles entry
/// 2. Update profile pnpm-workspace.yaml: add plugin source to `packages:`
/// 3. Run `pnpm install` in the profile directory to hoist deps
///
/// This avoids ERR_PNPM_ADDING_TO_ROOT because `pnpm install` (not `pnpm add`)
/// doesn't refuse to touch the workspace root — it only installs what's
/// declared in the manifest.
fn register_plugin_directly(
    profile_dir: &Path,
    plugin_source: &Path,
    plugin_name: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let manifest_path = profile_dir.join("package.json");
    let workspace_manifest = profile_dir.join("pnpm-workspace.yaml");
    if !manifest_path.is_file() {
        return Err(format!("profile at {} is missing package.json", profile_dir.display()));
    }

    // --- Step 1: Update package.json ---
    let manifest_text =
        fs::read_to_string(&manifest_path).map_err(|error| format!("cannot read package.json: {error}"))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .map_err(|error| format!("cannot parse package.json: {error}"))?;

    // Add to dependencies
    let needs_install = {
        let mut deps = match manifest
            .get("dependencies")
            .and_then(serde_json::Value::as_object)
        {
            Some(obj) => obj.clone(),
            None => serde_json::Map::new(),
        };
        let mut changed = false;
        match deps.get(plugin_name) {
            None | Some(serde_json::Value::Null) => {
                deps.insert(plugin_name.to_string(), serde_json::Value::String("workspace:*".to_owned()));
                changed = true;
            }
            Some(existing) if existing.as_str() == Some("workspace:*") => {}
            _ => {
                deps.insert(plugin_name.to_string(), serde_json::Value::String("workspace:*".to_owned()));
                changed = true;
            }
        }
        if changed {
            manifest["dependencies"] = serde_json::Value::Object(deps);
        }
        changed
    };

    // Add to dsh.profile.bundles if not already listed
    let needs_bundle = {
        let bundles = manifest
            .pointer_mut("/dsh/profile/bundles")
            .and_then(serde_json::Value::as_array_mut);
        if let Some(bundles_arr) = bundles {
            !bundles_arr.iter().any(|b| b.as_str() == Some(plugin_name))
        } else {
            false
        }
    };

    if needs_bundle {
        // Add plugin to dsh.profile.bundles
        let bundles = manifest
            .pointer_mut("/dsh/profile/bundles")
            .and_then(serde_json::Value::as_array_mut);
        if let Some(bundles_arr) = bundles {
            bundles_arr.push(serde_json::Value::String(plugin_name.to_string()));
        }
    }

    if needs_install || needs_bundle {
        let serialized = serde_json::to_string_pretty(&manifest)
            .unwrap_or_else(|_| manifest_text.to_owned());
        fs::write(&manifest_path, serialized)
            .map_err(|error| format!("cannot write package.json: {error}"))?;
    }

    // --- Step 2: Update pnpm-workspace.yaml ---
    let plugin_source_string = plugin_source.to_string_lossy().into_owned();
    if workspace_manifest.is_file() {
        let ws_text = fs::read_to_string(&workspace_manifest)
            .map_err(|error| format!("cannot read workspace yaml: {error}"))?;
        let mut updated = ws_text.clone();
        if !updated.contains(&plugin_source_string) {
            updated = inject_workspace_package(&updated, &plugin_source_string);
        }
        // Ensure build scripts are allowed (node-pty etc.)
        if !updated.contains("dangerouslyAllowAllBuilds") {
            let trailing = if updated.ends_with('\n') { "" } else { "\n" };
            updated.push_str(&format!("{trailing}dangerouslyAllowAllBuilds: true\n"));
        }
        if updated != ws_text {
            fs::write(&workspace_manifest, &updated)
                .map_err(|error| format!("cannot write workspace yaml: {error}"))?;
        }
    } else {
        // Create minimal workspace yaml
        let content = format!(
            "packages:\n  - .\n  - {}\nnodeLinker: hoisted\n\
             ignore-workspace-root-check: true\ndangerouslyAllowAllBuilds: true\n",
            plugin_source_string
        );
        fs::write(&workspace_manifest, content)
            .map_err(|error| format!("cannot create workspace yaml: {error}"))?;
    }

    // --- Step 3: Run pnpm install in profile dir ---
    let pnpm = resolve_toolchain("pnpm")?;
    task.log(&format!("installing plugin dependencies for {plugin_name}"));
    let task_record = task.manager.task(&task.task_id)?;
    let install_spec = ProcessSpec::new(pnpm.path.clone())
        .args([
            "--dir",
            profile_dir.to_string_lossy().as_ref(),
            "install",
            "--no-frozen-lockfile",
        ])
        .policy(pnpm_policy(&pnpm))
        .kind(ExecutionKind::Logged)
        .log_path(&task_record.log_path);
    let mut install_logged =
        run_logged(&install_spec, "pnpm install").map_err(|error| {
            format!("cannot start pnpm install: {error}")
        })?;
    let status = install_logged
        .wait_or_kill(
            &TaskCancel(Some(task)),
            Duration::from_secs(900),
            "installing plugin dependencies",
        )
        .map_err(|error| format!("pnpm install: {error}"))?;
    if !status.success() {
        return Err(format!("pnpm install for plugin {plugin_name} exited with {status}"));
    }
    Ok(())
}

/// Append an extra `packages:` entry to a `pnpm-workspace.yaml` whose
/// `packages:` list is a YAML block sequence. If the file uses a different
/// layout (no top-level `packages:` key, inline flow sequence, etc.) the
/// original is returned untouched so the caller can surface a clearer
/// error. The new entry is anchored right after the last existing `- `
/// line of the block sequence — never at end-of-file — so YAML keeps
/// treating every `- ` line as a member of the `packages:` list.
fn inject_workspace_package(workspace_text: &str, new_path: &str) -> String {
    let Some(packages_index) = workspace_text.find("packages:") else {
        return workspace_text.to_owned();
    };
    let after_packages = &workspace_text[packages_index + "packages:".len()..];
    let first_significant_offset = after_packages
        .chars()
        .position(|character| !character.is_whitespace())
        .unwrap_or(after_packages.len());
    let first_significant = after_packages[first_significant_offset..]
        .chars()
        .next()
        .unwrap_or('\0');
    let key_column = workspace_text[..packages_index]
        .rfind('\n')
        .map(|new_line| packages_index - new_line - 1)
        .unwrap_or(packages_index);
    let block_indent: String = " ".repeat(key_column + 2);
    if first_significant == '[' {
        let close = after_packages.find(']').unwrap_or(after_packages.len());
        let inside = &after_packages[first_significant_offset + 1..close];
        let entries: Vec<&str> = inside
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect();
        let rebuilt_entries = entries
            .iter()
            .map(|entry| format!("{block_indent}- {entry}\n"))
            .collect::<String>();
        let tail = &after_packages[close + 1..];
        let rebuilt = format!(
            "packages:\n{rebuilt_entries}{block_indent}- {new_path}\n{tail}"
        );
        let prefix = &workspace_text[..packages_index];
        return format!("{prefix}{rebuilt}");
    }
    if first_significant != '-' {
        // Unknown shape — return unchanged so the caller can surface a
        // clearer diagnostic than a half-rewrite would.
        return workspace_text.to_owned();
    }
    // Block sequence: walk the lines after `packages:`, find the last
    // `- ` line that has the same indent as the first, and insert the
    // new entry immediately after it. Lines that don't start with the
    // expected indent (or start with `-`) end the sequence.
    let lines: Vec<&str> = workspace_text.lines().collect();
    let packages_line_index = workspace_text[..packages_index].matches('\n').count();
    let mut last_member_index: Option<usize> = None;
    for (offset, line) in lines.iter().enumerate().skip(packages_line_index + 1) {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('-') {
            if indented_match(indent, &block_indent) {
                last_member_index = Some(offset);
                continue;
            }
            break;
        }
        if indent < block_indent.len() {
            break;
        }
        // A non-`-` line at the same indent is a sibling key (e.g.
        // `nodeLinker:`) — the block sequence is over.
        break;
    }
    let Some(last_member_index) = last_member_index else {
        // Empty list — emit the very first entry directly under the key.
        let mut output = String::new();
        for (offset, line) in lines.iter().enumerate() {
            if offset == packages_line_index {
                output.push_str(line);
                output.push('\n');
                output.push_str(&block_indent);
                output.push_str("- ");
                output.push_str(new_path);
                output.push('\n');
            } else {
                output.push_str(line);
                if offset + 1 < lines.len() {
                    output.push('\n');
                }
            }
        }
        if !workspace_text.ends_with('\n') && !output.ends_with('\n') {
            output.push('\n');
        }
        return output;
    };
    let mut output = String::new();
    for (offset, line) in lines.iter().enumerate() {
        if offset == last_member_index {
            output.push_str(line);
            output.push('\n');
            output.push_str(&block_indent);
            output.push_str("- ");
            output.push_str(new_path);
        } else {
            output.push_str(line);
        }
        if offset + 1 < lines.len() {
            output.push('\n');
        }
    }
    if workspace_text.ends_with('\n') && !output.ends_with('\n') {
        output.push('\n');
    }
    if !workspace_text.ends_with('\n') && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Whether `observed` is a leading-indent run that matches `expected`
/// (which is itself a whitespace-only string). Both strings are non-empty
/// here because the caller knows the first `-` line already has the
/// expected indent.
fn indented_match(observed: usize, expected: &str) -> bool {
    observed == expected.len()
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

#[cfg(test)]
mod tests {
    use super::inject_workspace_package;

    #[test]
    fn inject_workspace_package_appends_to_block_sequence() {
        let input = "packages:\n  - .\n\nnodeLinker: hoisted\n";
        let output = inject_workspace_package(input, "/path/to/plugin");
        // Trailing newline is preserved exactly as in the input.
        assert_eq!(
            output,
            "packages:\n  - .\n  - /path/to/plugin\n\nnodeLinker: hoisted\n"
        );
    }

    #[test]
    fn inject_workspace_package_handles_inline_flow_sequence() {
        // Entries are preserved verbatim from the source flow sequence so
        // pnpm sees the same intent; the only change is the conversion to
        // a block sequence with the new entry appended.
        let input = "packages: ['.', '../shared']\n";
        let output = inject_workspace_package(input, "/path/to/plugin");
        assert_eq!(
            output,
            "packages:\n  - '.'\n  - '../shared'\n  - /path/to/plugin\n\n"
        );
    }

    #[test]
    fn inject_workspace_package_passes_through_unknown_layout() {
        let input = "nodeLinker: hoisted\n";
        let output = inject_workspace_package(input, "/path/to/plugin");
        assert_eq!(output, input);
    }
}
