//! Sealed-template construction. A sealed template is a source recipe. pnpm
//! workspace links are created only when a container has reached its final
//! directory, because Windows junction targets are absolute.

use crate::{
    containers::create_profile_manifest,
    lifecycle::materialize_bundled_sandbox_plugin,
    toolchains::{pnpm_policy, resolve_toolchain, run_logged, TaskCancel},
    versions::{list_prepared_bases, prepared_base_for_version, PreparedBaseRecord},
};
use box_api::{BuildImageRequest, CreateTemplateContainerRequest, TemplateInfo};
use box_foundation::copy_tree::copy_tree_following;
use box_extensions::transfer::extract_extension_tarball;
use box_foundation::{
    atomic_write_json, now_seconds, write_template_manifest, PublishedResourceKind, RuntimeLayout,
    TemplateManifest, STORAGE_SCHEMA_VERSION,
};
use box_runtime::process::{ExecutionKind, ProcessSpec};
use box_scheduler::TaskContext;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

mod client_artifacts;
mod pnpm_approval;
mod sealed_index;

// sealed_index is the storage, pnpm_approval is the supply-chain gate,
// client_artifacts is the platform marker, and this file is the build / export /
// import orchestration over all three. The globs below keep every existing
// `crate::sealed::x` path working.
pub(crate) use client_artifacts::*;
// Same shape as the dispatch globs: the items are `pub(self)`, so this
// re-exports nothing (the warning) while being the only import that puts
// them in scope and the only path that makes them reachable from outside.
#[allow(unused_imports)]
pub(crate) use pnpm_approval::*;
pub(crate) use sealed_index::*;

/// Build a sealed template through DSH's own plugin command. The Boxfile
/// source is converted only where its grammar needs normalization; package
/// resolution itself remains pnpm's responsibility.
pub(crate) fn build_sealed_template_from_script(
    request: BuildImageRequest,
    task: &TaskContext,
) -> Result<(), String> {
    task.update("Parsing build script", 5);
    let script_path = PathBuf::from(&request.script_path);
    let script_text = fs::read_to_string(&script_path)
        .map_err(|error| format!("cannot read script {}: {error}", script_path.display()))?;
    let base_dir = script_path.parent().unwrap_or_else(|| Path::new("."));
    let script = box_image::parse_script(&script_text, base_dir)
        .map_err(|error| format!("script parse error: {error}"))?;
    if script.base_template.is_some() {
        return Err("sealed-template build does not yet accept template inheritance; use the prepared Harness ref directly".to_owned());
    }
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let version = script.harness_ref.as_deref().unwrap_or("latest");
    let base = prepared_base_for_version(&root, version)?;
    let mut plugin_sources = Vec::new();
    for operation in &script.ops {
        let box_image::ImageOp::Add { kind, source, .. } = operation;
        if !matches!(kind, box_image::AddKind::Plugin) {
            return Err("sealed-template build currently supports ADD plugin only; import skills/data after the container-copy migration".to_owned());
        }
        plugin_sources.push(pnpm_plugin_spec(source)?);
    }
    // A lifecycle script is executable code. A Boxfile must opt in to it
    // explicitly; a bare ADD never weakens pnpm's default policy.
    let allowed_build_sources = script
        .labels
        .get("dshbox.allow-build")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let name = request
        .container_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&script.name);
    seal_template(
        &root,
        name,
        &base,
        &script.profile,
        &plugin_sources,
        &allowed_build_sources,
        &script.env,
        &script_text,
        task,
    )
    .map(|_| ())
}

/// List every runnable template. A prepared Harness base is directly runnable
/// with no plugins; a sealed template is a prepared base plus a Boxfile plugin
/// recipe. Both the CLI and desktop consume this one list before creating a
/// container, so discovery and execution use the same semantics.
pub(crate) fn list_sealed_templates() -> Result<Vec<TemplateInfo>, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let mut templates = BTreeMap::new();
    for base in list_prepared_bases(&root)? {
        if let Some(template) = prepared_base_template_info(base) {
            templates.insert(template.name.clone(), template);
        }
    }
    for record in read_index(&layout)?.into_values() {
        let manifest: TemplateManifest = serde_json::from_str(
            &fs::read_to_string(Path::new(&record.directory).join("manifest.json"))
                .map_err(|error| format!("cannot read sealed template manifest: {error}"))?,
        )
        .map_err(|error| format!("cannot parse sealed template manifest: {error}"))?;
        manifest.validate()?;
        templates.insert(
            record.name.clone(),
            TemplateInfo {
                name: record.name,
                id: record.id,
                harness_ref: Some(manifest.source_ref),
                profile: record.profile,
                built: true,
            },
        );
    }
    Ok(templates.into_values().collect())
}

/// A pulled Harness root carries no profile tree of its own; it is always the
/// web profile. Sealed templates built from it record their own.
const PREPARED_BASE_PROFILE: &str = "web";

fn prepared_base_template_info(base: PreparedBaseRecord) -> Option<TemplateInfo> {
    let directory = Path::new(&base.directory);
    if !directory.join("manifest.json").is_file()
        || !directory.join("harness/package.json").is_file()
    {
        return None;
    }
    Some(TemplateInfo {
        name: base.source_ref.clone(),
        id: base.id,
        harness_ref: Some(base.source_ref),
        profile: PREPARED_BASE_PROFILE.to_owned(),
        built: false,
    })
}

/// Resolve a template *name* to its root directory and profile.
///
/// A prepared Harness base is listed as a runnable template before anything is
/// built from it, so a name has to resolve against both indices: sealed-only
/// resolution rejects the very root `dshbox pull` just produced, which is the
/// template a user is most likely to inspect first.
pub(crate) fn template_root_by_name(root: &str, name: &str) -> Result<(String, PathBuf), String> {
    match sealed_template_by_name(root, name) {
        Ok(record) => Ok((record.profile, PathBuf::from(record.directory))),
        Err(sealed_error) => {
            let base = list_prepared_bases(root)?
                .into_iter()
                .find(|base| base.source_ref == name)
                .ok_or(sealed_error)?;
            Ok((
                PREPARED_BASE_PROFILE.to_owned(),
                PathBuf::from(base.directory),
            ))
        }
    }
}

pub(crate) fn read_sealed_template(name: &str) -> Result<String, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let record = sealed_template_by_name(&root, name)?;
    fs::read_to_string(Path::new(&record.directory).join("boxfile.dsh"))
        .map_err(|error| format!("cannot read sealed template `{name}`: {error}"))
}

pub(crate) fn sealed_template_info(name: &str) -> Result<serde_json::Value, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let record = sealed_template_by_name(&root, name)?;
    let manifest: TemplateManifest = serde_json::from_str(
        &fs::read_to_string(Path::new(&record.directory).join("manifest.json"))
            .map_err(|error| format!("cannot read sealed template manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse sealed template manifest: {error}"))?;
    manifest.validate()?;
    Ok(serde_json::json!({
        "name": record.name,
        "id": record.id,
        "built": true,
        "base": manifest.source_ref,
        "baseId": record.base_id,
        "profile": record.profile,
        "schemaVersion": manifest.schema_version,
        "createdAt": record.created_at,
        "sizeBytes": record.size_bytes,
        "pluginSources": record.plugin_sources,
    }))
}

/// Delete a sealed template only when it is not referenced by a container.
/// Containers contain physical copies, but retaining this guard prevents a
/// surprise loss of the source template in the management UI.
pub(crate) fn remove_sealed_template(name: &str) -> Result<(), String> {
    if !box_foundation::is_safe_identifier(name) {
        return Err("invalid template name".to_owned());
    }
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let mut index = read_index(&layout)?;
    let record = index
        .get(name)
        .cloned()
        .ok_or_else(|| format!("sealed template `{name}` not found"))?;
    if template_is_used_by_container(&layout, name)? {
        return Err(format!(
            "template `{name}` is still referenced by a container"
        ));
    }
    let directory = PathBuf::from(&record.directory);
    if directory.is_dir() {
        fs::remove_dir_all(&directory)
            .map_err(|error| format!("cannot remove sealed template `{name}`: {error}"))?;
    }
    index.remove(name);
    write_index(&layout, &index)
}

/// Export the complete immutable tree, including the materialized
/// `node_modules` and profile, so importing it never needs a registry or a
/// shared runtime. This supersedes the legacy archive that reconstructed a
/// template from separate runtime and repository records.
pub(crate) fn export_sealed_template(
    name: &str,
    destination: Option<String>,
) -> Result<String, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let record = sealed_template_by_name(&root, name)?;
    let destination = destination.unwrap_or_else(|| format!("./{name}.dsh.tar.gz"));
    let destination_path = PathBuf::from(&destination);
    if destination_path
        .extension()
        .and_then(|value| value.to_str())
        != Some("gz")
    {
        return Err("template export destination must end in .tar.gz".to_owned());
    }
    if let Some(parent) = destination_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let output = fs::File::create(&destination_path)
        .map_err(|error| format!("cannot create template archive: {error}"))?;
    let encoder = flate2::write::GzEncoder::new(output, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let descriptor = serde_json::json!({
        "format": "dshbox-sealed-template",
        "version": 1,
        "record": record,
    });
    let descriptor = serde_json::to_vec_pretty(&descriptor).map_err(|error| error.to_string())?;
    let mut header = tar::Header::new_gnu();
    header.set_size(descriptor.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "export.json", descriptor.as_slice())
        .map_err(|error| format!("cannot write export descriptor: {error}"))?;
    archive
        .append_dir_all("sealed", Path::new(&record.directory))
        .map_err(|error| format!("cannot archive sealed template: {error}"))?;
    archive
        .into_inner()
        .map_err(|error| format!("cannot finalize template archive: {error}"))?
        .finish()
        .map_err(|error| format!("cannot finalize template gzip stream: {error}"))?;
    Ok(destination_path.to_string_lossy().into_owned())
}

pub(crate) fn import_sealed_template(
    archive: &str,
    name: Option<String>,
) -> Result<String, String> {
    if archive.trim().is_empty() {
        return Err("template archive path cannot be empty".to_owned());
    }
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let extraction = layout.create_staging_dir("template_import")?;
    let result = (|| -> Result<String, String> {
        extract_extension_tarball(Path::new(archive), &extraction)?;
        let descriptor: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(extraction.join("export.json"))
                .map_err(|error| format!("cannot read template export descriptor: {error}"))?,
        )
        .map_err(|error| format!("cannot parse template export descriptor: {error}"))?;
        if descriptor["format"].as_str() != Some("dshbox-sealed-template")
            || descriptor["version"].as_u64() != Some(1)
        {
            return Err(
                "unsupported template archive format; export it again with schema-10 DSH Box"
                    .to_owned(),
            );
        }
        let mut record: SealedTemplateRecord = serde_json::from_value(descriptor["record"].clone())
            .map_err(|error| format!("invalid sealed template record: {error}"))?;
        record.name = name.unwrap_or(record.name);
        if !box_foundation::is_safe_identifier(&record.name) {
            return Err("invalid template name".to_owned());
        }
        let payload = extraction.join("sealed");
        let manifest: TemplateManifest = serde_json::from_str(
            &fs::read_to_string(payload.join("manifest.json"))
                .map_err(|error| format!("template archive is missing manifest: {error}"))?,
        )
        .map_err(|error| format!("invalid sealed template manifest: {error}"))?;
        manifest.validate()?;
        if manifest.kind != PublishedResourceKind::SealedTemplate || manifest.id != record.id {
            return Err("template archive manifest does not match its sealed record".to_owned());
        }
        if !payload.join("harness/apps/web/dist/index.html").is_file() {
            return Err("template archive is missing the prepared frontend".to_owned());
        }
        let mut index = read_index(&layout)?;
        if index.contains_key(&record.name) {
            return Err(format!("sealed template `{}` already exists", record.name));
        }
        let publish = layout.create_staging_dir("template_publish")?;
        copy_tree_following(&payload, &publish)
            .map_err(|error| format!("cannot materialize imported template: {error}"))?;
        let destination =
            layout.sealed_template_dir(record.id.strip_prefix("sealed-").unwrap_or(&record.id))?;
        layout.publish_staged_tree(&publish, &destination)?;
        record.directory = destination.to_string_lossy().into_owned();
        index.insert(record.name.clone(), record.clone());
        write_index(&layout, &index)?;
        Ok(record.name)
    })();
    let _ = fs::remove_dir_all(&extraction);
    result
}

/// Materialize one sealed recipe into a standalone container directory. The
/// package-manager work happens after the directory has its final name, so
/// pnpm's Windows junctions can never refer to disposable staging paths.
pub(crate) fn create_container_from_sealed(
    request: &CreateTemplateContainerRequest,
    task: &TaskContext,
) -> Result<box_containers::DshContainer, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let (source, profile, plugin_sources, template_name, sealed_id, direct_root) =
        match sealed_template_by_name(&root, &request.template) {
            Ok(sealed) => {
                if let Some(requested_profile) = request.profile.as_deref() {
                    if requested_profile != sealed.profile {
                        return Err(format!(
                            "sealed template `{}` contains profile `{}`; creating it with `{requested_profile}` would require mutation",
                            sealed.name, sealed.profile
                        ));
                    }
                }
                (
                    PathBuf::from(sealed.directory),
                    sealed.profile,
                    sealed.plugin_sources,
                    sealed.name,
                    Some(sealed.id),
                    false,
                )
            }
            Err(_) => {
                let reference = box_dsh_versions::parse_template_ref(&request.template)
                    .map_err(|_| format!("template `{}` not found", request.template))?;
                let base = prepared_base_for_version(&root, &reference.version)?;
                (
                    PathBuf::from(base.directory),
                    request.profile.clone().unwrap_or_else(|| "web".to_owned()),
                    Vec::new(),
                    request.template.clone(),
                    None,
                    true,
                )
            }
        };
    if !source.join("manifest.json").is_file() || !source.join("harness/package.json").is_file() {
        return Err(format!("template `{template_name}` is incomplete"));
    }
    let timestamp = now_seconds();
    let id = format!("container-{timestamp}-{}", task.task_id);
    if !box_foundation::is_safe_identifier(&id) {
        return Err("generated unsafe container id".to_owned());
    }
    let destination = layout.root().join("instances").join(&id);
    if destination.exists() {
        return Err(format!(
            "container destination already exists: {}",
            destination.display()
        ));
    }
    let staged = layout.create_staging_dir("container_create")?;
    task.update(
        if direct_root {
            "Copying prepared template source"
        } else {
            "Copying sealed recipe"
        },
        20,
    );
    task.log(if direct_root {
        "copying the prepared Harness source into the container"
    } else {
        "copying the sealed template tree (Harness source plus its built client artifacts) into the container"
    });
    let copy_started = std::time::Instant::now();
    let copy_result = if direct_root {
        copy_source_tree(&source, &staged)
    } else {
        copy_tree_following(&source, &staged).map_err(|error| error.to_string())
    }
    .map_err(|error| format!("cannot copy sealed template: {error}"));
    if let Err(error) = copy_result {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    task.log(&format!("copied in {}s", copy_started.elapsed().as_secs()));
    if direct_root {
        create_profile_manifest(&staged, &profile)?;
    }
    let manifest: TemplateManifest = serde_json::from_str(
        &fs::read_to_string(staged.join("manifest.json"))
            .map_err(|error| format!("cannot read sealed template manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse sealed template manifest: {error}"))?;
    manifest.validate()?;
    let version = box_dsh_versions::parse_template_ref(&manifest.source_ref)
        .map(|reference| reference.version)
        .unwrap_or_else(|_| "latest".to_owned());
    layout.publish_staged_tree(&staged, &destination)?;
    fs::create_dir_all(destination.join("logs"))
        .map_err(|error| format!("cannot create container logs: {error}"))?;
    let prepare_result = prepare_container_at_final_path(
        &destination,
        &profile,
        &plugin_sources,
        &manifest.source_commit,
        task,
    );
    if let Err(error) = prepare_result {
        let diagnostic_dir = layout.root().join("logs").join("containers");
        let diagnostic_log = diagnostic_dir.join(format!("{id}-prepare.log"));
        let _ = fs::create_dir_all(&diagnostic_dir);
        let _ = fs::copy(
            destination.join("logs").join("prepare.log"),
            &diagnostic_log,
        );
        let _ = fs::remove_dir_all(&destination);
        return Err(format!(
            "{error}; retained diagnostics at {}",
            diagnostic_log.display()
        ));
    }
    task.update("Writing container state", 84);
    for directory in ["workspace", "logs", "state"] {
        fs::create_dir_all(destination.join(directory)).map_err(|error| error.to_string())?;
    }
    let metadata = serde_json::json!({
        "id": id,
        "name": request.name,
        "version": version,
        "profile": profile,
        "template": template_name,
        "sealedTemplate": sealed_id,
        "source": "container-local",
        "env": manifest.env.clone(),
    });
    atomic_write_json(&destination.join("container.json"), &metadata)?;
    task.update("Container prepared", 100);
    Ok(box_containers::DshContainer {
        id,
        name: request.name.clone(),
        version,
        profile,
        template: Some(template_name),
        runtime: None,
        directory: destination.to_string_lossy().into_owned(),
        status: "stopped".to_owned(),
        env: manifest.env,
    })
}

/// Prepared bases own one stable, reusable Harness tool tree. It is never
/// copied into a sealed template or container; it only lets `pnpm dsh plugin`
/// run while building a profile recipe. This removes a full 900+ package
/// materialization from every subsequent template build.
fn ensure_prepared_base_tools(
    harness: &Path,
    pnpm: &crate::toolchains::ResolvedToolchain,
    log_path: &Path,
    task: &TaskContext,
) -> Result<(), String> {
    if harness.join("node_modules/tsx/package.json").is_file() {
        return Ok(());
    }
    let modules = harness.join("node_modules");
    if modules.exists() {
        fs::remove_dir_all(&modules).map_err(|error| {
            format!(
                "cannot discard stale prepared-base package graph {}: {error}",
                modules.display()
            )
        })?;
    }
    task.update("Materializing cached DSH tool tree", 42);
    run_pnpm_command(
        pnpm,
        harness,
        ["install", "--offline"],
        log_path,
        task,
        "prepared-base tool dependency install",
        None,
        None,
    )
}

/// Build and publish a sealed template from one already-validated prepared
/// base. It deliberately omits node_modules: pnpm must create its workspace
/// links in an instance's final path, not in a staging directory.
pub(crate) fn seal_template(
    root: &str,
    name: &str,
    base: &PreparedBaseRecord,
    profile: &str,
    plugin_sources: &[String],
    allowed_build_sources: &[String],
    env: &BTreeMap<String, String>,
    boxfile_source: &str,
    task: &TaskContext,
) -> Result<SealedTemplateRecord, String> {
    if !box_foundation::is_safe_identifier(profile) || name.trim().is_empty() {
        return Err("sealed template requires a non-empty name and safe profile".to_owned());
    }
    let layout = RuntimeLayout::new(root);
    layout.initialize_schema_10()?;
    let base_directory = PathBuf::from(&base.directory);
    let base_harness = base_directory.join("harness");
    if !base_harness.join("package.json").is_file() {
        return Err(format!("prepared base {} is incomplete", base.id));
    }
    let staged = layout.create_staging_dir("template_build")?;
    let staged_harness = staged.join("harness");
    let result = (|| -> Result<SealedTemplateRecord, String> {
        // A base prepared before Box built client artifacts at base time gets
        // them here, once: every template from it then carries a run-ready tree.
        ensure_base_client_artifacts(&base_directory, &base.source_commit, task)?;
        task.update("Copying prepared base", 20);
        task.log("copying the prepared Harness tree into the template");
        let copy_started = std::time::Instant::now();
        copy_source_tree(&base_harness, &staged_harness)
            .map_err(|error| format!("cannot copy prepared base: {error}"))?;
        task.log(&format!("copied in {}s", copy_started.elapsed().as_secs()));
        fs::write(staged.join("boxfile.dsh"), boxfile_source)
            .map_err(|error| format!("cannot preserve Boxfile source: {error}"))?;
        create_profile_manifest(&staged, profile)?;
        seed_profile_recipe(
            &staged,
            &base_harness,
            profile,
            plugin_sources,
            allowed_build_sources,
            task,
        )?;
        task.update("Validating sealed recipe", 72);
        if !staged_harness.join("package.json").is_file()
            || staged_harness.join("node_modules").exists()
        {
            return Err("sealed template recipe has an invalid Harness source tree".to_owned());
        }
        if !client_artifacts_present(&staged_harness, &base.source_commit) {
            return Err(
                "sealed template is missing the client artifacts the prepared base built"
                    .to_owned(),
            );
        }
        let created_at = now_seconds();
        let identity = format!(
            "{}\n{}\n{}\n{}\n{}",
            name,
            base.id,
            profile,
            plugin_sources.join("\n"),
            env.iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let digest = box_dsh_versions::template_content_hash(&identity);
        let id = format!("sealed-{digest}");
        let manifest = TemplateManifest {
            schema_version: STORAGE_SCHEMA_VERSION,
            kind: PublishedResourceKind::SealedTemplate,
            id: id.clone(),
            source_ref: base.source_ref.clone(),
            source_commit: base.source_commit.clone(),
            node_version: "bundled".to_owned(),
            pnpm_version: "bundled".to_owned(),
            base_id: Some(base.id.clone()),
            plugin_artifact_ids: Vec::new(),
            plugin_sources: plugin_sources.to_vec(),
            env: env.clone(),
            harness_digest: digest.clone(),
            profile_digest: Some(digest.clone()),
            size_bytes: tree_size(&staged)?,
            validated_at: created_at,
        };
        write_template_manifest(&staged, &manifest)?;
        Ok(SealedTemplateRecord {
            id,
            name: name.trim().to_owned(),
            base_id: base.id.clone(),
            profile: profile.to_owned(),
            directory: String::new(),
            plugin_artifact_ids: Vec::new(),
            plugin_sources: plugin_sources.to_vec(),
            created_at,
            size_bytes: manifest.size_bytes,
        })
    })();
    let mut record = match result {
        Ok(record) => record,
        Err(error) => {
            let diagnostic_dir = layout.root().join("logs").join("templates");
            let diagnostic_log = diagnostic_dir.join(format!("{}-build.log", task.task_id));
            let _ = fs::create_dir_all(&diagnostic_dir);
            let retained = fs::copy(staged.join("prepare.log"), &diagnostic_log).is_ok();
            let _ = fs::remove_dir_all(&staged);
            return if retained {
                Err(format!(
                    "{error}; retained diagnostics at {}",
                    diagnostic_log.display()
                ))
            } else {
                Err(error)
            };
        }
    };
    let destination =
        layout.sealed_template_dir(record.id.strip_prefix("sealed-").unwrap_or(&record.id))?;
    layout.publish_staged_tree(&staged, &destination)?;
    record.directory = destination.to_string_lossy().into_owned();
    let mut index = read_index(&layout)?;
    if index.contains_key(&record.name) {
        return Err(format!("sealed template `{}` already exists", record.name));
    }
    index.insert(record.name.clone(), record.clone());
    write_index(&layout, &index)?;
    task.log(&format!(
        "sealed template `{}` published at {}",
        record.name, record.directory
    ));
    task.update("Sealed template ready", 100);
    Ok(record)
}

fn prepare_container_at_final_path(
    directory: &Path,
    profile: &str,
    plugin_sources: &[String],
    source_commit: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let harness = directory.join("harness");
    let profile_home = directory.join("profile").to_string_lossy().into_owned();
    let pnpm = resolve_toolchain("pnpm")?;
    let log_path = directory.join("logs").join("prepare.log");
    task.update("Installing DSH dependencies", 38);
    task.log("installing DSH dependencies offline from the pnpm store");
    let install_started = std::time::Instant::now();
    run_pnpm_command(
        &pnpm,
        &harness,
        ["install", "--offline"],
        &log_path,
        task,
        "container offline dependency install",
        None,
        None,
    )?;
    task.log(&format!(
        "dependencies linked in {}s",
        install_started.elapsed().as_secs()
    ));

    if !plugin_sources.is_empty() {
        let profile_dir = directory.join("profile/profiles").join(profile);
        task.update("Materializing cached plugin recipe", 62);
        task.log(&format!(
            "installing {} profile plugin source(s) offline",
            plugin_sources.len()
        ));
        run_pnpm_command(
            &pnpm,
            &profile_dir,
            ["install", "--offline", "--frozen-lockfile"],
            &log_path,
            task,
            "container offline plugin install",
            None,
            None,
        )?;
    }

    // The client artifacts are built once, while the prepared base is created,
    // and copied here with the rest of the tree. Only a template that predates
    // that (or one whose tree lost them) still needs the build.
    if client_artifacts_present(&harness, source_commit) {
        task.update("Using the template's client artifacts", 72);
        task.log("client artifacts came with the template; skipping the build");
    } else {
        task.update("Building DSH frontend", 72);
        task.log(
            "this template has no prebuilt client artifacts, so they are built now \
             (native addon, libs, web frontend — a few minutes). Rebuild the template \
             with `dshbox build` to make container creation a copy.",
        );
        let build_started = std::time::Instant::now();
        run_pnpm_command(
            &pnpm,
            &harness,
            ["run", "build"],
            &log_path,
            task,
            "container frontend build",
            Some(profile_home),
            Some(source_commit.to_owned()),
        )?;
        task.log(&format!(
            "client build finished in {}s",
            build_started.elapsed().as_secs()
        ));
        write_client_artifact_marker(&harness, source_commit)?;
    }
    if !harness.join("apps/web/dist/index.html").is_file()
        || !harness.join("node_modules/tsx/package.json").is_file()
    {
        return Err("container preparation is missing DSH build outputs".to_owned());
    }
    task.update("Installing DSH Box sandbox plugin", 82);
    materialize_bundled_sandbox_plugin(directory, profile, Some(task))?;
    Ok(())
}

fn run_pnpm_command<const N: usize>(
    pnpm: &crate::toolchains::ResolvedToolchain,
    harness: &Path,
    args: [&str; N],
    log_path: &Path,
    task: &TaskContext,
    label: &str,
    dsh_home: Option<String>,
    client_commit: Option<String>,
) -> Result<(), String> {
    // Windows Defender real-time scan can race pnpm's package.json reads and
    // surface as a generic [UNKNOWN] UNKNOWN: open '<file>' exit. Retry a
    // few times with exponential backoff before declaring the command dead,
    // so transient locks (Defender, indexing service, lingering AV handles)
    // do not break the container prepare pipeline.
    const MAX_ATTEMPTS: u32 = 4;
    let mut last_error: Option<String> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match run_pnpm_command_once(
            pnpm,
            harness,
            args,
            log_path,
            task,
            label,
            &dsh_home,
            &client_commit,
        ) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let transient = attempt < MAX_ATTEMPTS && log_indicates_transient_io(log_path);
                if !transient {
                    return Err(error);
                }
                let backoff_ms = 500_u64 * 2u64.saturating_pow(attempt - 1);
                last_error = Some(error);
                task.update(
                    &format!(
                        "{label}: transient file lock on attempt {attempt}/{MAX_ATTEMPTS}, \
                         retrying in {backoff_ms}ms"
                    ),
                    u8::MAX,
                );
                std::thread::sleep(Duration::from_millis(backoff_ms));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| format!("{label} failed after {MAX_ATTEMPTS} attempts")))
}

fn run_pnpm_command_once<const N: usize>(
    pnpm: &crate::toolchains::ResolvedToolchain,
    harness: &Path,
    args: [&str; N],
    log_path: &Path,
    task: &TaskContext,
    label: &str,
    dsh_home: &Option<String>,
    client_commit: &Option<String>,
) -> Result<(), String> {
    let harness_arg = harness.to_string_lossy().into_owned();
    let mut policy = pnpm_policy(pnpm)?;
    if let Some(home) = dsh_home {
        policy = policy.task_override("DSH_HOME", home);
    }
    if let Some(commit) = client_commit {
        policy = policy.task_override("DSH_CLIENT_COMMIT_HASH", commit);
    }
    let spec = ProcessSpec::new(pnpm.path.clone())
        .args(&pnpm.arguments)
        .args(["--dir", harness_arg.as_str()])
        .args(args)
        .cwd(harness)
        .policy(policy)
        .kind(ExecutionKind::Logged)
        .log_path(log_path);
    let mut process = run_logged(&spec, label)?;
    let status = process
        .wait_or_kill(&TaskCancel(Some(task)), Duration::from_secs(900), label)
        .map_err(|error| format!("{label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        log_command_tail(log_path, task, 20);
        Err(format!("{label} failed; inspect {}", log_path.display()))
    }
}

/// Inspect the tail of `log_path` for the Windows-specific file-lock
/// signatures that warrant a transient retry: `EBUSY`, `EPERM`,
/// `EACCES`, or pnpm's generic `[UNKNOWN] UNKNOWN` that wraps them when
/// reading package.json files inside `node_modules/`.
/// Append the tail of a command transcript to the task log. The full output
/// stays in the transcript file, but a failure that only says "inspect
/// /path/prepare.log" makes the UI log useless when the run is the thing the
/// user is watching.
pub(crate) fn log_command_tail(log_path: &Path, task: &TaskContext, lines: usize) {
    let Ok(contents) = fs::read_to_string(log_path) else {
        return;
    };
    let tail: Vec<&str> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(lines)
        .collect();
    for line in tail.into_iter().rev() {
        task.log(line);
    }
}

fn log_indicates_transient_io(log_path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return false;
    };
    let tail_start = contents.len().saturating_sub(4096);
    let tail = &contents[tail_start..];
    tail.contains("EBUSY")
        || tail.contains("EPERM")
        || tail.contains("EACCES")
        || tail.contains("[UNKNOWN] UNKNOWN")
}

/// Copy a reusable source tree while intentionally excluding package-manager
/// graphs and VCS metadata. `node_modules` cannot be dereferenced safely:
/// pnpm workspace links may contain cycles and Windows junctions have absolute
/// targets. Any remaining link outside those directories is copied by value.
fn copy_source_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("node_modules") | Some(".git") | Some(".pnpm")
        ) {
            continue;
        }
        let source_path = entry.path();
        let target_path = destination.join(&name);
        let metadata = fs::metadata(&source_path)
            .map_err(|error| format!("cannot inspect source {}: {error}", source_path.display()))?;
        if metadata.is_dir() {
            copy_source_tree(&source_path, &target_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &target_path).map_err(|error| {
                format!("cannot copy source {}: {error}", source_path.display())
            })?;
        }
    }
    Ok(())
}

fn tree_size(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            total = total.saturating_add(tree_size(&entry.path())?);
        } else if kind.is_file() {
            total =
                total.saturating_add(entry.metadata().map_err(|error| error.to_string())?.len());
        }
    }
    Ok(total)
}

fn template_is_used_by_container(layout: &RuntimeLayout, name: &str) -> Result<bool, String> {
    let instances = layout.root().join("instances");
    if !instances.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(&instances).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata_path = entry.path().join("container.json");
        let Ok(body) = fs::read_to_string(metadata_path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        if metadata["template"].as_str() == Some(name) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
#[path = "sealed_tests.rs"]
mod tests;