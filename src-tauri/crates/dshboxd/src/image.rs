//! Build orchestration for the daemon: parse a `.dsh`/boxfile, materialise
//! each ADD into the Repository, install into the freshly-created container,
//! and (optionally) write a portable `.dshimage` archive.
//!
//! This mirrors the desktop's `image.rs` build path without any Tauri
//! dependency; the daemon owns the single execution context.

use crate::containers::create_dsh_container_sync;
use crate::extensions::{import_into_repository, link_repository_extension};
use box_dsh_versions::{
    collect_unreferenced_template_hash, harness_template_path, read_template_index,
    template_content_path, template_index_path,
    template_storage_root, templates_directory,
    write_template_index, TemplateEntry,
};
use box_extensions::transfer::{
    archive_content_root, copy_extension_source, extract_extension_tarball,
};
use box_extensions::{repository_root, scan_repository, ExtensionKind, RepositoryExtension};
use box_foundation::{mirror_url, now_seconds, read_config};
use box_image::{
    compile_manifest, parse_script, write_dshimage, ImageManifest, ImageOp, ParsedSource,
};
use box_runtime::shallow_clone_with_cancel;
use box_scheduler::TaskContext;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// Maximum depth of the FROM template chain.
pub(crate) const MAX_TEMPLATE_CHAIN_DEPTH: usize = 4;

/// System plugin scope: `@deepseek-ai/*` plugins ship with the harness.
pub(crate) const SYSTEM_PLUGIN_SCOPE: &str = "deepseek-ai";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateInfo {
    pub(crate) name: String,
    /// Content-addressable identifier (fnv1a64 hex of the script body).
    pub(crate) id: String,
    pub(crate) harness_ref: Option<String>,
    pub(crate) profile: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildImageRequest {
    pub(crate) script_path: String,
    pub(crate) output_path: Option<String>,
    pub(crate) container_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateTemplateContainerRequest {
    pub(crate) name: String,
    pub(crate) template: String,
    pub(crate) profile: Option<String>,
}

struct ResolvedBase {
    harness_url: String,
    harness_ref: Option<String>,
    profile: String,
}

fn resolve_template_base(
    root: &str,
    script: &box_image::ImageScript,
    seen: &mut HashSet<String>,
    depth: usize,
) -> Result<ResolvedBase, String> {
    if let Some(base) = &script.base_template {
        if depth >= MAX_TEMPLATE_CHAIN_DEPTH {
            return Err(format!(
                "template chain exceeds {MAX_TEMPLATE_CHAIN_DEPTH} levels at `{base}`"
            ));
        }
        if !seen.insert(base.clone()) {
            return Err(format!("template cycle detected at `{base}`"));
        }
        let path = templates_directory(root).join(format!("{base}.dsh"));
        if !path.is_file() {
            return Err(format!("base template not found: {base} ({})", path.display()));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read base template `{base}`: {error}"))?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let parsed = parse_script(&text, base_dir)
            .map_err(|error| format!("base template `{base}` parse error: {error}"))?;
        let resolved = resolve_template_base(root, &parsed, seen, depth + 1)?;
        Ok(ResolvedBase {
            harness_url: resolved.harness_url,
            harness_ref: resolved.harness_ref,
            profile: script.profile.clone(),
        })
    } else {
        Ok(ResolvedBase {
            harness_url: script.harness_url.clone(),
            harness_ref: script.harness_ref.clone(),
            profile: script.profile.clone(),
        })
    }
}

/// Materialize a script into a real container:
/// 1. parse the script
/// 2. create the container (profile + DSH version come from the script)
/// 3. for each ADD: pull or copy into Repository, then copy into the
///    freshly-created container
/// 4. optionally write a `.dshimage` archive with embedded blobs
pub(crate) fn build_image_from_script(
    request: BuildImageRequest,
    task: &TaskContext,
) -> Result<(), String> {
    task.update("Parsing build script", 5);
    let script_path = PathBuf::from(&request.script_path);
    let script_text = std::fs::read_to_string(&script_path)
        .map_err(|error| format!("cannot read script `{}`: {error}", script_path.display()))?;
    let base_dir = script_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut script = parse_script(&script_text, &base_dir)
        .map_err(|error| format!("script parse error: {error}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let resolved = resolve_template_base(&root, &script, &mut HashSet::new(), 0)?;
    script.harness_url = resolved.harness_url;
    script.harness_ref = resolved.harness_ref;
    script.profile = resolved.profile;

    let container_name = request
        .container_name
        .clone()
        .unwrap_or_else(|| script.name.clone());
    task.update("Creating container", 15);
    let container =
        create_dsh_container_sync(&container_name, &script.harness_ref.clone().unwrap_or_else(|| "latest".to_owned()), &script.profile)?;
    record_container_template(&container, &script)?;
    task.update("Materialising extensions", 25);

    let inline_blobs = materialize_ops(task, &container, &script)?;

    if let Some(output_path) = request.output_path.as_ref() {
        task.update("Writing image archive", 96);
        let manifest = compile_manifest(&script, now_seconds());
        write_archive(&manifest, &inline_blobs, Path::new(output_path), task)?;
        task.log(&format!("wrote image to {output_path}"));
    }

    task.update("Image built", 100);
    task.log(&format!(
        "container {} ready with {} extension(s)",
        container.id,
        script.ops.len()
    ));
    Ok(())
}

/// Create a container from a local template (`.dsh` in the templates
/// directory), then start it — mirrors `dshbox run` and the UI's
/// create-then-start flow.
pub(crate) fn materialize_template_container(
    request: CreateTemplateContainerRequest,
    task: &TaskContext,
) -> Result<box_containers::DshContainer, String> {
    if !is_safe_template_name(&request.template) {
        return Err("invalid template name".to_owned());
    }
    task.update("Reading template", 10);
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Resolve through the index so any template name (including
    // `github.com/<owner>/<repo>:<tag>` aliases) finds the right script
    // body in the content-addressable hash directory. The legacy
    // flat-file fallback keeps builds working before the user has
    // re-pulled the corresponding harness.
    let template_path = lookup_template_path(&root, &request.template)?;
    let text = std::fs::read_to_string(&template_path)
        .map_err(|error| format!("cannot read template: {error}"))?;
    let base_dir = template_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut script = parse_script(&text, base_dir)
        .map_err(|error| format!("template parse error: {error}"))?;
    task.update("Resolving template chain", 20);
    let resolved = resolve_template_base(&root, &script, &mut HashSet::new(), 0)?;
    script.harness_url = resolved.harness_url;
    script.harness_ref = resolved.harness_ref;
    if let Some(profile) = &request.profile {
        script.profile = profile.clone();
    }
    task.update("Creating container", 30);
    let version = script.harness_ref.clone().unwrap_or_else(|| "latest".to_owned());
    let container =
        create_dsh_container_sync(&request.name, &version, &script.profile)?;
    record_container_template(&container, &script)?;
    task.update("Materialising extensions", 45);
    let _ = materialize_ops(task, &container, &script)?;
    task.log(&format!(
        "container {} created from template {} with {} extension(s)",
        container.id,
        request.template,
        script.ops.len()
    ));
    Ok(container)
}

fn record_container_template(
    container: &box_containers::DshContainer,
    script: &box_image::ImageScript,
) -> Result<(), String> {
    let template = script
        .base_template
        .clone()
        .or_else(|| script.harness_ref.clone());
    let Some(template) = template else {
        return Ok(());
    };
    let metadata_path = Path::new(&container.directory).join("container.json");
    let mut metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&metadata_path)
            .map_err(|error| format!("cannot read container: {error}"))?,
    )
    .map_err(|error| format!("cannot parse container: {error}"))?;
    metadata["template"] = serde_json::Value::String(template);
    std::fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot save container: {error}"))
}

fn materialize_ops(
    task: &TaskContext,
    container: &box_containers::DshContainer,
    script: &box_image::ImageScript,
) -> Result<Vec<(String, PathBuf)>, String> {
    let mut inline_blobs: Vec<(String, PathBuf)> = Vec::new();
    let total_ops = script.ops.len().max(1);
    for (index, op) in script.ops.iter().enumerate() {
        let ImageOp::Add { kind, source, .. } = op;
        let progress = 25 + ((index * 70) / total_ops);
        task.update(
            format!(
                "Installing {}/{} ({})",
                index + 1,
                total_ops,
                describe_parsed(source),
            ),
            progress as u8,
        );
        let Some(ext_kind) = kind.to_extension_kind() else {
            crate::data::materialize_data_add(task, container, source)?;
            continue;
        };
        match source {
            ParsedSource::BareName { name, scope, version } => {
                if scope.as_deref() == Some(SYSTEM_PLUGIN_SCOPE) {
                    task.log(&format!(
                        "skipping system plugin @{SYSTEM_PLUGIN_SCOPE}/{name}: provided by the harness"
                    ));
                    continue;
                }
                install_from_repository(
                    task,
                    container,
                    &script.profile,
                    &ext_kind,
                    name,
                    scope.as_deref(),
                    version.as_deref(),
                )?;
            }
            ParsedSource::Github { url, ref_ } => {
                let entry = fetch_github_extension(task, url, ref_.as_deref(), &ext_kind)?;
                install_from_repository_entry(task, container, &script.profile, &entry)?;
            }
            ParsedSource::Tarball { url, local } => {
                let entry = fetch_tarball_extension(task, url, *local, &ext_kind)?;
                if !*local {
                    inline_blobs.push((
                        entry.content_digest.clone(),
                        PathBuf::from(&entry.source_path),
                    ));
                }
                install_from_repository_entry(task, container, &script.profile, &entry)?;
            }
            ParsedSource::LocalDir { path } => {
                let entry = fetch_local_dir_extension(task, path, &ext_kind)?;
                inline_blobs.push((
                    entry.content_digest.clone(),
                    PathBuf::from(&entry.source_path),
                ));
                install_from_repository_entry(task, container, &script.profile, &entry)?;
            }
        }
    }
    Ok(inline_blobs)
}

fn describe_parsed(value: &ParsedSource) -> String {
    match value {
        ParsedSource::Github { url, ref_ } => match ref_ {
            Some(reference) => format!("{url}@{reference}"),
            None => url.clone(),
        },
        ParsedSource::Tarball { url, .. } => url.clone(),
        ParsedSource::LocalDir { path } => path.to_string_lossy().into_owned(),
        ParsedSource::BareName { name, scope, version } => {
            let head = match scope {
                Some(scope) => format!("@{scope}/{name}"),
                None => name.clone(),
            };
            match version {
                Some(version) => format!("{head}@{version}"),
                None => head,
            }
        }
    }
}

/// List the local templates (hash-addressed under `<root>/templates/<id>/`,
/// looked up through `<root>/state/template-index.json`). Lazily migrates
/// any leftover `templates/*.dsh` scripts left by older builds into the new
/// layout so a single list call is enough to recover a clean state.
pub(crate) fn list_templates() -> Result<Vec<TemplateInfo>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    migrate_legacy_template_files(&root)?;
    let index = read_template_index(&root);
    let mut templates: Vec<TemplateInfo> = index
        .values()
        .map(|entry| TemplateInfo {
            name: entry.name.clone(),
            id: entry.id.clone(),
            harness_ref: entry.harness_ref.clone(),
            profile: entry.profile.clone(),
        })
        .collect();
    templates.sort_by(|a, b| match (a.name == "latest", b.name == "latest") {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(templates)
}

/// Move legacy `templates/<name>.dsh` scripts into the content-addressable
/// hash layout, register them in the index, and delete the originals. Runs
/// during `list_templates` so the migration is transparent, but only on the
/// first call (once `state/template-index.json` exists, we are running on
/// the new layout and the flat-file aliases left by `pull_template` must
/// NOT be re-imported as new entries — that would duplicate every pull).
fn migrate_legacy_template_files(root: &str) -> Result<(), String> {
    if template_index_path(root).is_file() {
        return Ok(());
    }
    let directory = templates_directory(root);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let mut index = read_template_index(root);
    let mut changed = false;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("dsh") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let stem = stem.to_owned();
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let id = box_dsh_versions::template_content_hash(&text);
        let dest_dir = template_storage_root(root).join(&id);
        std::fs::create_dir_all(&dest_dir).map_err(|error| error.to_string())?;
        std::fs::write(dest_dir.join("script.dsh"), &text)
            .map_err(|error| format!("cannot migrate {stem}: {error}"))?;
        let entry = TemplateEntry {
            name: stem,
            id,
            harness_ref: None,
            profile: String::new(),
            imported_at: now_seconds(),
            from_ref: None,
        };
        index.insert(entry.name.clone(), entry);
        std::fs::remove_file(&path).map_err(|error| error.to_string())?;
        changed = true;
    }
    if changed {
        write_template_index(root, &index)?;
    }
    Ok(())
}

fn is_safe_template_name(name: &str) -> bool {
    // `name` is a user-facing alias (a slash-separated ref like
    // `github.com/<owner>/<repo>:<tag>`) that we round-trip into the index
    // manifest. The actual template body lives in a content-addressable
    // hash directory, so the name does not need to be a valid filesystem
    // identifier — just long enough, free of control characters, and not
    // empty.
    !name.is_empty()
        && name.len() <= 256
        && name.chars().all(|ch| {
            !ch.is_control() && !matches!(ch, '\0' | '\n' | '\r' | '\t')
        })
        && !name.contains("..")
}

/// Read the contents of a local template (resolved through the index).
pub(crate) fn read_template(name: &str) -> Result<String, String> {
    if !is_safe_template_name(name) {
        return Err("invalid template name".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = lookup_template_path(&root, name)?;
    std::fs::read_to_string(&path).map_err(|error| format!("cannot read template: {error}"))
}

/// Resolve a template's on-disk path via the index, transparently falling
/// back to the legacy flat layout so older installs still work.
fn lookup_template_path(root: &str, name: &str) -> Result<PathBuf, String> {
    migrate_legacy_template_files(root)?;
    let index = read_template_index(root);
    if let Some(entry) = index.get(name) {
        let path = template_content_path(root, &entry.id);
        if path.is_file() {
            return Ok(path);
        }
    }
    let legacy = harness_template_path(root, name);
    if legacy.is_file() {
        return Ok(legacy);
    }
    Err(format!("template not found: {name}"))
}

/// Import a template tarball (the format produced by `export_template`).
/// The archive is unpacked to a staging directory, validated, then stored
/// under `<root>/templates/<content-hash>/script.dsh` and registered in
/// the index under `name` (or the archive stem when not provided).
pub(crate) fn import_template(archive: &str, name: Option<String>) -> Result<String, String> {
    if archive.trim().is_empty() {
        return Err("template archive path cannot be empty".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let archive_path = Path::new(archive);
    if !archive_path.is_file() {
        return Err(format!("template archive not found: {archive}"));
    }
    let staging = std::env::temp_dir()
        .join(format!("dshbox-template-import-{}-{}", std::process::id(), now_seconds()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    if let Err(error) = extract_extension_tarball(archive_path, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    // The archive must contain exactly one .dsh file at the top level.
    let dsh_files: Vec<PathBuf> = std::fs::read_dir(&staging)
        .map_err(|error| format!("cannot read staged archive: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|value| value.to_str()) == Some("dsh"))
        .collect();
    if dsh_files.is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("template archive contains no .dsh file".to_owned());
    }
    if dsh_files.len() > 1 {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("template archive contains multiple .dsh files".to_owned());
    }
    let source = dsh_files.into_iter().next().expect("non-empty");
    let target_name = match name {
        Some(name) if !name.is_empty() => name,
        _ => source
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
            .ok_or_else(|| "template archive is missing a filename stem".to_owned())?,
    };
    if !is_safe_template_name(&target_name) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("invalid template name `{target_name}`"));
    }
    if read_template_index(&root).contains_key(&target_name) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "template `{target_name}` already exists; remove it first or pick a different --name"
        ));
    }
    let text = std::fs::read_to_string(&source).map_err(|error| error.to_string())?;
    // Best-effort metadata from the script header; falls back to the
    // tarball's filename when parsing fails (e.g. unsupported directives).
    let (harness_ref, profile) = match parse_script(&text, Path::new(".")) {
        Ok(script) => (script.harness_ref, script.profile),
        Err(_) => (None, String::new()),
    };
    let entry = box_dsh_versions::write_template_with_entry(
        &root,
        &target_name,
        &text,
        harness_ref,
        &profile,
        Some(format!("imported:{archive}")),
        now_seconds(),
    )?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(entry.name)
}

/// Export a local template to a gzip tarball. The default destination is
/// `./<name>.dsh.tar.gz` in the current working directory; an explicit path
/// is honoured verbatim (must end in `.tar.gz`).
pub(crate) fn export_template(name: &str, destination: Option<String>) -> Result<String, String> {
    if !is_safe_template_name(name) {
        return Err("invalid template name".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let source = lookup_template_path(&root, name)?;
    if !source.is_file() {
        return Err(format!("template not found: {name}"));
    }
    let dest_str = match destination {
        Some(value) if !value.is_empty() => value,
        _ => format!("./{name}.dsh.tar.gz"),
    };
    let dest_path = Path::new(&dest_str);
    if dest_path.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("template export destination must end in .tar.gz".to_owned());
    }
    if let Some(parent) = dest_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }
    let output = std::fs::File::create(dest_path).map_err(|error| format!("cannot create template archive: {error}"))?;
    let encoder = flate2::write::GzEncoder::new(output, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive
        .append_path_with_name(&source, format!("{name}.dsh"))
        .map_err(|error| format!("cannot archive template: {error}"))?;
    archive
        .into_inner()
        .map_err(|error| format!("cannot finalize template archive: {error}"))?
        .finish()
        .map_err(|error| format!("cannot finalize gzip stream: {error}"))?;
    Ok(dest_path.to_string_lossy().into_owned())
}

/// Remove a local template (hash directory + index entry). Refuses if any
/// non-deleted container records this template as its base, mirroring the
/// reference-counting guard the plugin resource uses (deleting a template
/// still in use would leave the container pointing at a missing script).
pub(crate) fn remove_template(name: &str) -> Result<(), String> {
    if !is_safe_template_name(name) {
        return Err("invalid template name".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut index = read_template_index(&root);
    let entry = index
        .get(name)
        .cloned()
        .or_else(|| {
            // Legacy flat layout: the name maps straight to a .dsh file.
            let legacy = harness_template_path(&root, name);
            if legacy.is_file() {
                Some(TemplateEntry {
                    name: name.to_owned(),
                    id: String::new(),
                    harness_ref: None,
                    profile: String::new(),
                    imported_at: 0,
                    from_ref: None,
                })
            } else {
                None
            }
        })
        .ok_or_else(|| format!("template not found: {name}"))?;
    let containers = box_containers::scan_containers(&root)
        .map_err(|error| format!("cannot scan containers: {error}"))?;
    let mut used_by: Vec<String> = containers
        .into_values()
        .filter(|container| container.template.as_deref() == Some(name))
        .map(|container| container.id)
        .collect();
    used_by.sort();
    if !used_by.is_empty() {
        return Err(format!(
            "template `{name}` is used by {} container(s) ({}); remove them first",
            used_by.len(),
            used_by.join(", ")
        ));
    }
    index.remove(name);
    write_template_index(&root, &index)?;
    if !entry.id.is_empty() {
        collect_unreferenced_template_hash(&root, &entry.id, &index);
    } else {
        let legacy = harness_template_path(&root, name);
        let _ = std::fs::remove_file(&legacy);
    }
    Ok(())
}

fn install_from_repository(
    task: &TaskContext,
    container: &box_containers::DshContainer,
    profile: &str,
    kind: &ExtensionKind,
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let full_name = match scope {
        Some(scope) => format!("@{scope}/{name}"),
        None => name.to_string(),
    };
    let entries = scan_repository(Path::new(&root));
    let entry = entries
        .into_iter()
        .find(|entry| {
            entry.kind == *kind
                && entry.name == full_name
                && match (version, &entry.version) {
                    (None, _) => true,
                    (Some(want), Some(have)) => want == have,
                    (Some(_), None) => false,
                }
        })
        .ok_or_else(|| {
            format!(
                "plugin `{full_name}` not found in repository. Import it first with `ADD plugin {full_name}` from a source you control."
            )
        })?;
    install_from_repository_entry(task, container, profile, &entry)
}

fn install_from_repository_entry(
    task: &TaskContext,
    container: &box_containers::DshContainer,
    profile: &str,
    entry: &RepositoryExtension,
) -> Result<(), String> {
    link_repository_extension(&container.id, Some(profile), &entry.id, task)
}

fn fetch_github_extension(
    task: &TaskContext,
    url: &str,
    reference: Option<&str>,
    _kind: &ExtensionKind,
) -> Result<RepositoryExtension, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let staging_id = format!("github-{}-{}", std::process::id(), now_seconds());
    let staging = repository_root(Path::new(&root))
        .join("staging")
        .join(&staging_id);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let destination = staging.join("source");
    let config = read_config()?;
    let target = mirror_url(url, config.github_mirror.as_deref());
    task.log(&format!("cloning GitHub repository {url}"));
    let cancelled = task.clone();
    shallow_clone_with_cancel(&target, &destination, reference, move || cancelled.cancelled())?;
    import_into_repository(task, &destination)
}

fn fetch_tarball_extension(
    task: &TaskContext,
    source: &str,
    local: bool,
    _kind: &ExtensionKind,
) -> Result<RepositoryExtension, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let staging_id = format!("tar-{}-{}", std::process::id(), now_seconds());
    let staging = repository_root(Path::new(&root))
        .join("staging")
        .join(&staging_id);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let destination = staging.join("source");
    if local {
        let archive = PathBuf::from(source);
        if !archive.is_file() {
            return Err(format!("tarball `{source}` does not exist"));
        }
        task.log(&format!("extracting local tarball {}", archive.display()));
        extract_extension_tarball(&archive, &destination)?;
    } else {
        task.log(&format!("downloading tarball {source}"));
        download_remote_tarball(source, &destination)?;
    }
    let content_root = archive_content_root(&destination)?;
    import_into_repository(task, &content_root)
}

fn fetch_local_dir_extension(
    task: &TaskContext,
    source: &Path,
    _kind: &ExtensionKind,
) -> Result<RepositoryExtension, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let staging_id = format!("dir-{}-{}", std::process::id(), now_seconds());
    let staging = repository_root(Path::new(&root))
        .join("staging")
        .join(&staging_id);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let destination = staging.join("source");
    task.log(&format!("copying local directory {}", source.display()));
    copy_extension_source(source, &destination)?;
    import_into_repository(task, &destination)
}

pub(crate) fn download_remote_tarball(url: &str, destination: &Path) -> Result<(), String> {
    use std::io::Write;
    let response = reqwest::blocking::get(url)
        .map_err(|error| format!("download {url}: {error}"))?;
    let bytes = response
        .bytes()
        .map_err(|error| format!("download {url}: {error}"))?;
    let archive_path = destination
        .parent()
        .unwrap_or(destination)
        .join("archive.bin");
    let mut file = std::fs::File::create(&archive_path)
        .map_err(|error| format!("cannot create archive staging: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot write archive: {error}"))?;
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    extract_extension_tarball(&archive_path, destination)?;
    let _ = std::fs::remove_file(&archive_path);
    Ok(())
}

fn write_archive(
    manifest: &ImageManifest,
    blobs: &[(String, PathBuf)],
    output: &Path,
    task: &TaskContext,
) -> Result<(), String> {
    let blob_refs: Vec<(String, &Path)> = blobs
        .iter()
        .map(|(digest, path)| (digest.clone(), path.as_path()))
        .collect();
    write_dshimage(manifest, &blob_refs, output)
        .map_err(|error| format!("cannot write image archive: {error}"))?;
    task.log(&format!(
        "embedded {} blob(s) into {}",
        blob_refs.len(),
        output.display()
    ));
    Ok(())
}
