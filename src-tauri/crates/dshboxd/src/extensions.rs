//! Repository extension management for the daemon: copy into containers,
//! import/export entries, remove entries, and direct `dsh plugin add`.
//! Mirrors the desktop's `extensions.rs` paths without Tauri deps.

use crate::bundles::{install_container_plugin, install_container_skill};
use crate::toolchains::{pnpm_policy, resolve_toolchain, run_logged, TaskCancel};
use box_dsh_versions::version_directory as dsh_version_directory;
use box_extensions::transfer::{
    append_plugin_archive, archive_content_root, copy_extension_source, extract_extension_tarball,
};
use box_extensions::{
    detect_extension_kind, read_bundles, remove_plugin_record,
    repository_root, scan_repository, write_bundles, write_repository_index, ExtensionKind,
    RepositoryExtension,
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

/// True when the plugin's package.json declares any runtime or build
/// dependency. A pure-patch plugin (e.g. a Cordis `patch: "./x.yml"`
/// with no imports) has nothing to install and skips the pnpm boot —
/// saves the bundled-runtime boot for unit tests with empty fixtures
/// and for plugins that genuinely have no npm deps to fetch.
fn plugin_declares_deps(directory: &Path) -> bool {
    let Ok(manifest_text) = fs::read_to_string(directory.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest_text) else {
        return false;
    };
    let non_empty = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|object| !object.is_empty())
    };
    non_empty("dependencies") || non_empty("devDependencies")
}

/// Immutable local package input for a repository plugin. The source tree is
/// retained only for inspection/export; template construction must consume
/// this archive instead of a workspace or absolute source path.
pub(crate) fn repository_plugin_artifact(entry: &RepositoryExtension) -> Result<PathBuf, String> {
    if entry.kind != ExtensionKind::Plugin {
        return Err("only plugins have installable artifacts".to_owned());
    }
    PathBuf::from(&entry.source_path)
        .parent()
        .map(|directory| directory.join("artifact.tgz"))
        .ok_or("repository plugin source has no parent".to_owned())
}

fn package_repository_plugin(
    task: &TaskContext,
    source: &Path,
    artifact: &Path,
    name: &str,
) -> Result<(), String> {
    if artifact.is_file() {
        return Ok(());
    }
    let artifact_parent = artifact.parent().ok_or("plugin artifact has no parent")?;
    let staging = artifact_parent.join(format!(".pack-{}", task.task_id));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let npm = resolve_toolchain("npm")?;
    let log_path = task.manager.task(&task.task_id)?.log_path;
    task.update("Packing plugin artifact", 82);
    task.log(&format!("packing immutable local artifact for {name}"));
    let staging_arg = staging.to_string_lossy().into_owned();
    let spec = ProcessSpec::new(npm.path.clone())
        .args(&npm.arguments)
        .args(["pack", "--pack-destination", staging_arg.as_str()])
        .cwd(source)
        .policy(pnpm_policy(&npm))
        .kind(ExecutionKind::Logged)
        .log_path(&log_path);
    let mut process = run_logged(&spec, "plugin artifact pack")?;
    let status = process
        .wait_or_kill(
            &TaskCancel(Some(task)),
            Duration::from_secs(300),
            "packing plugin artifact",
        )
        .map_err(|error| format!("plugin artifact pack: {error}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("plugin artifact pack failed; inspect {log_path}"));
    }
    let packed = fs::read_dir(&staging)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_file() && matches!(path.extension().and_then(|ext| ext.to_str()), Some("tgz") | Some("gz")))
        .ok_or("npm pack produced no tarball")?;
    fs::rename(&packed, artifact).map_err(|error| {
        format!(
            "cannot publish plugin artifact {}: {error}",
            artifact.display()
        )
    })?;
    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

pub(crate) fn repository_metadata(
    kind: &ExtensionKind,
    source: &Path,
) -> Result<(String, Option<String>, Option<String>), String> {
    match kind {
        ExtensionKind::Skill => {
            let content =
                fs::read_to_string(source.join("SKILL.md")).map_err(|error| error.to_string())?;
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
                &fs::read_to_string(source.join("package.json"))
                    .map_err(|error| error.to_string())?,
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

/// Look up an existing repository entry by its package identity:
/// `(kind, name, version)`. Plugins require an exact version match because
/// the `img-<id>` rows are per-version (v0.12.2 and v0.12.3 of the same
/// plugin are distinct entries the user can keep side by side); skills are
/// not versioned, so the version slot is ignored. Missing or invalid
/// entries (`diagnostic` set) are skipped so a stale failure does not
/// poison the cache hit path.
pub(crate) fn find_repository_entry_by_identity(
    root: &str,
    kind: &ExtensionKind,
    name: &str,
    version: Option<&str>,
) -> Option<RepositoryExtension> {
    scan_repository(Path::new(root)).into_iter().find(|entry| {
        entry.kind == *kind
            && entry.name == name
            && entry.diagnostic.is_none()
            && match (kind, version, &entry.version) {
                (ExtensionKind::Plugin, want, have) => want == have.as_deref(),
                (ExtensionKind::Skill, _, _) => true,
            }
    })
}

/// Install a repository entry into a container profile (plugin) or skill root.
/// Plugins are installed via `dsh plugin add` with the repository source path;
/// skills are copied into a per-container directory.
///
/// `template_id` identifies the template the link was triggered by when
/// this call originates from `materialize_built_template`. Passing
/// `Some(id)` records the template as an owner of the repository entry;
/// passing `None` skips the bookkeeping (the caller is a direct
/// `plugin install <container>` and only the container owns the entry).
pub(crate) fn link_repository_extension(
    container_id: &str,
    profile: Option<&str>,
    repository_id: &str,
    template_id: Option<&str>,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Reconcile before mutating so the on-disk owner map matches the
    // canonical sources after any prior crash or stale write.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
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
    task.update("Installing repository extension", 25);
    match entry.kind {
        ExtensionKind::Plugin => {
            let profile = profile.ok_or("plugin installation requires a profile")?;
            let profile_dir = PathBuf::from(&container.directory)
                .join("profile/profiles")
                .join(profile);
            if !profile_dir.join("package.json").is_file() {
                return Err(format!("profile not found: {profile}"));
            }
            // Install the plugin into the profile via DSH's tooling. The
            // plugin source lives in the shared repository directory so
            // multiple containers can reference the same entry.
            let source_path = PathBuf::from(&entry.source_path);
            crate::bundles::install_container_plugin(
                &container,
                profile,
                "repository",
                &entry.id,
                source_path,
                task,
            )?;
            // A plugin linked from a template records the template as
            // one of its template-side owners. Direct `plugin install
            // <container>` flows through `install_container_extension`
            // / `container_plugin_add` and records the container
            // owner there instead — passing `template_id = None` here
            // skips the bookkeeping on that path.
            if let Some(template_id) = template_id {
                box_extensions::add_reference_owner(
                    Path::new(&root),
                    &entry.id,
                    box_extensions::ReferenceKind::Template,
                    template_id,
                )?;
            }
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
            crate::bundles::install_container_skill(&container, "repository", &entry.id, staging, task)?
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
    let (name, version, description) = repository_metadata(&kind, source)
        .map_err(|error| format!("repository metadata: {error}"))?;
    // Cache hit: the same package (`name[@version]` for plugin, `name` for
    // skill) already lives in the repository. The local source is the same
    // artefact under hash storage, so reuse the existing entry instead of
    // creating a duplicate `img-<task_id>` row. The caller's staging clone
    // gets discarded by the next staging timestamp — no leak.
    if let Some(existing) =
        find_repository_entry_by_identity(&root, &kind, &name, version.as_deref())
    {
        // Repositories created before dependency installation was added have
        // no `node_modules/`. Repair such a cache hit before reusing it so a
        // later container link never builds against an empty dependency tree.
        let existing_source = Path::new(&existing.source_path);
        if matches!(kind, ExtensionKind::Plugin)
            && plugin_declares_deps(existing_source)
            && !existing_source.join("node_modules").is_dir()
        {
            install_plugin_dependencies(task, existing_source, &name, version.as_deref())?;
        }
        if matches!(kind, ExtensionKind::Plugin) {
            let artifact = repository_plugin_artifact(&existing)?;
            package_repository_plugin(task, existing_source, &artifact, &existing.name)?;
        }
        let kind_label = match existing.kind {
            ExtensionKind::Plugin => "plugin",
            ExtensionKind::Skill => "skill",
        };
        task.log(&format!(
            "reusing cached {} {}{}",
            kind_label,
            existing.name,
            existing
                .version
                .as_deref()
                .map(|v| format!("@{v}"))
                .unwrap_or_default(),
        ));
        return Ok(existing);
    }
    // `img-<task_id>` is unique per task, but a single build task imports
    // multiple plugins. If the task-level id is already on disk (a
    // previous ADD in the same build), append a counter to avoid the
    // collision. The repository index is keyed by `name+version` anyway;
    // the `img-*` name is just a filesystem convenience.
    let base_id = format!("img-{}", task.task_id);
    let mut entry_id = base_id.clone();
    let mut counter = 1usize;
    let destination = loop {
        let dest = repository_root(Path::new(&root))
            .join(match kind {
                ExtensionKind::Plugin => "plugins",
                ExtensionKind::Skill => "skills",
            })
            .join(&entry_id)
            .join("source");
        if dest.exists() {
            counter += 1;
            entry_id = format!("{}-{}", base_id, counter);
            continue;
        }
        break dest;
    };
    fs::create_dir_all(destination.parent().ok_or("destination has no parent")?)
        .map_err(|error| error.to_string())?;
    copy_extension_source(source, &destination)?;
    // Plugins need their dependencies installed at import time so the
    // repository entry owns a ready-to-link source. Code subdirectory
    // links into a container keep inode sharing with the repo entry,
    // and the plugin's build (tsdown/rolldown) resolves transitive
    // deps from the symlinked source files — if the repo entry has no
    // `node_modules/`, the resolver walks past the symlink and
    // externalises every dep it cannot find, leaving `require("clsx")`
    // (and friends) in the bundle. Installing here once keeps the
    // pnpm store layout intact so every later container reuses it.
    //
    // Plugins with no `dependencies` / `devDependencies` (pure-patch
    // plugins, or test fixtures) skip the pnpm boot — they have
    // nothing to install and would only pay the runtime-bootstrap
    // cost. The bundled runtime must already be initialised by the
    // caller when this branch fires, which is the production path.
    if matches!(kind, ExtensionKind::Plugin) && plugin_declares_deps(&destination) {
        install_plugin_dependencies(task, &destination, &name, version.as_deref())?;
    }
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
    let created = entries
        .into_iter()
        .find(|entry| entry.id == entry_id)
        .expect("entry we just pushed");
    if matches!(&created.kind, ExtensionKind::Plugin) {
        let artifact = repository_plugin_artifact(&created)?;
        if let Err(error) = package_repository_plugin(task, Path::new(&created.source_path), &artifact, &created.name) {
            let mut rollback = scan_repository(Path::new(&root));
            rollback.retain(|entry| entry.id != created.id);
            let _ = write_repository_index(Path::new(&root), &rollback);
            let _ = fs::remove_dir_all(Path::new(&created.source_path).parent().unwrap_or(Path::new(".")));
            return Err(error);
        }
    }
    Ok(created)
}

fn install_plugin_dependencies(
    task: &TaskContext,
    directory: &Path,
    name: &str,
    version: Option<&str>,
) -> Result<(), String> {
    let pnpm = resolve_toolchain("pnpm")?;
    // pnpm ≥10 blocks postinstall/build scripts of third-party
    // dependencies unless explicitly approved (ERR_PNPM_IGNORED_BUILDS
    // exits the install with status 1 — see the user-facing failure
    // "plugin ... dependency installation exited with exit status: 1").
    // DSH container plugins are expected to run their deps' build
    // scripts (native modules like `cpu-features`/`ssh2`/`cloudflared`
    // need them); the container is isolated, so we approve them
    // wholesale with `dangerouslyAllowAllBuilds: true`.
    //
    // Note: `onlyBuiltDependencies: ['*']` is a pnpm ≤9 setting; pnpm
    // 11 reads `dangerouslyAllowAllBuilds` instead (verified against
    // the bundled pnpm 11.7.0 — the old key is silently ignored and the
    // install still fails with ERR_PNPM_IGNORED_BUILDS).
    let workspace_manifest = directory.join("pnpm-workspace.yaml");
    // Rewrite whenever the file is missing OR was produced by an older
    // dshboxd (which wrote the pnpm-9 `onlyBuiltDependencies` key that
    // pnpm 11 silently ignores). Preserve a user-authored manifest that
    // already sets `dangerouslyAllowAllBuilds`.
    let needs_rewrite = match fs::read_to_string(&workspace_manifest) {
        Ok(content) => !content.contains("dangerouslyAllowAllBuilds"),
        Err(_) => true,
    };
    if needs_rewrite {
        fs::write(
            &workspace_manifest,
            "packages:\n  - .\n\nnodeLinker: hoisted\ndangerouslyAllowAllBuilds: true\n",
        )
        .map_err(|error| format!("cannot write workspace manifest: {error}"))?;
    }
    let task_record = task.manager.task(&task.task_id)?;
    let frozen = directory.join("pnpm-lock.yaml").is_file();
    task.log(&format!(
        "installing dependencies for {}{}",
        name,
        version.map(|v| format!("@{v}")).unwrap_or_default(),
    ));
    let install_spec = ProcessSpec::new(pnpm.path.clone())
        .args(&pnpm.arguments)
        .args([
            "--dir",
            directory.to_string_lossy().as_ref(),
            "install",
            "--force",
            if frozen {
                "--frozen-lockfile"
            } else {
                "--no-frozen-lockfile"
            },
        ])
        .policy(pnpm_policy(&pnpm))
        .kind(ExecutionKind::Logged)
        .log_path(&task_record.log_path);
    let mut install_logged = run_logged(&install_spec, "pnpm install").map_err(|error| {
        format!("cannot start plugin dependency install: {error}")
    })?;
    let status = install_logged
        .wait_or_kill(
            &TaskCancel(Some(task)),
            Duration::from_secs(900),
            "installing plugin dependencies",
        )
        .map_err(|error| format!("pnpm install: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "plugin {name} dependency installation exited with {status}"
        ))
    }
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
    // Reconcile first so a torn or stale references.json cannot block a
    // legitimate delete.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
    let mut entries = scan_repository(Path::new(&root));
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
        .ok_or("repository extension not found")?;
    let owners = box_extensions::read_references(Path::new(&root))
        .get(id)
        .cloned()
        .unwrap_or_default();
    if !owners.is_empty() {
        let mut parts = Vec::new();
        if !owners.templates.is_empty() {
            parts.push(format!(
                "{} template(s) [{}]",
                owners.templates.len(),
                owners.templates.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        return Err(format!(
            "repository extension `{}` is still referenced by {}; remove or detach them first",
            entry.name,
            parts.join(" and ")
        ));
    }
    fs::remove_dir_all(
        PathBuf::from(&entry.source_path)
            .parent()
            .ok_or("repository source has no parent")?,
    )
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
    // Reconcile first so `unused_repository_ids` reflects the canonical
    // truth, not whatever the previous run left on disk.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
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

    // Ensure the profile's pnpm-workspace.yaml allows build scripts
    // (dangerouslyAllowAllBuilds: true) so pnpm ≥10 does not block
    // native module compilation (e.g. node-pty) with ERR_PNPM_IGNORED_BUILDS.
    let profile_dir = PathBuf::from(&container.directory)
        .join("profile")
        .join("profiles")
        .join(profile);
    let workspace_manifest = profile_dir.join("pnpm-workspace.yaml");
    let needs_rewrite = match fs::read_to_string(&workspace_manifest) {
        Ok(content) => !content.contains("dangerouslyAllowAllBuilds"),
        Err(_) => true,
    };
    if needs_rewrite {
        let ws_content = if workspace_manifest.is_file() {
            let text = fs::read_to_string(&workspace_manifest)
                .map_err(|error| format!("cannot read workspace yaml: {error}"))?;
            let trailing = if text.ends_with('\n') { "" } else { "\n" };
            format!("{text}{trailing}dangerouslyAllowAllBuilds: true\n")
        } else {
            "packages:\n  - .\n\nnodeLinker: hoisted\ndangerouslyAllowAllBuilds: true\n".to_owned()
        };
        fs::write(&workspace_manifest, &ws_content)
            .map_err(|error| format!("cannot write workspace manifest: {error}"))?;
    }

    let task_record = task.manager.task(&task.task_id)?;
    let dsh_home = PathBuf::from(&container.directory).join("profile");
    let install_spec = ProcessSpec::new(pnpm.path.clone())
        .args(&pnpm.arguments)
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
        .policy(
            pnpm_policy(&pnpm)
                .task_override("DSH_HOME", dsh_home.to_string_lossy().into_owned()),
        )
        .kind(ExecutionKind::Logged)
        .log_path(&task_record.log_path);
    let mut install_logged = run_logged(&install_spec, "dsh plugin add")
        .map_err(|error| format!("cannot start plugin install: {error}"))?;
    let status = install_logged
        .wait_or_kill(
            &TaskCancel(Some(task)),
            Duration::from_secs(900),
            "installing plugin",
        )
        .map_err(|error| format!("dsh plugin add: {error}"))?;
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
    // Reconcile before any owner mutation so a stale file cannot cause
    // duplicate or phantom owners.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
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
        ExtensionKind::Plugin => {
            install_container_plugin(&container, profile, source_kind, source, extracted, task)
        }
    }?;
    // Container-side reference: only recorded when the source is a
    // shared repository entry. Local / tarball installs have no
    // `repository_id`, so the owner set never grows for them — they
    // also never show up as `unused_repository_ids`.
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
pub(crate) fn remove_repository_plugin(id: &str, profile: &str, name: &str) -> Result<(), String> {
    if !is_safe_identifier(id) || !is_safe_identifier(profile) || !is_safe_package_name(name) {
        return Err("invalid plugin removal request".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Reconcile first so the per-container record we are about to remove
    // is reflected in the owner set we're going to update.
    let _ = box_extensions::reconcile_owner_index(Path::new(&root));
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
    // Release the container owner BEFORE removing the per-container
    // record so the `repository_id` is still available. Local /
    // tarball installs have no `repository_id`, and the remove is a
    // no-op for them.
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

#[cfg(test)]
mod tests {
    use super::*;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use std::env;

    struct NoopNotifier;
    impl box_scheduler::TaskNotifier for NoopNotifier {
        fn stage(&self, _task_id: &str, _stage: &str, _progress: u8) {}
        fn log(&self, _task_id: &str, _line: &str) {}
    }

    fn sandbox(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("dshboxd-ext-{name}-{}", now_seconds()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(home: &Path, runtime: &Path) {
        let config_dir = home.join(".dsh-box");
        fs::create_dir_all(&config_dir).unwrap();
        let body = serde_json::json!({ "runtimeDirectory": runtime.to_string_lossy() });
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    fn test_task(runtime: &Path) -> TaskContext {
        let paths = BoxPaths {
            config: runtime.join("config.json"),
            runtime: Some(runtime.to_path_buf()),
        };
        let manager = TaskManager::default();
        let record = manager
            .enqueue(&paths, "test", Vec::new(), serde_json::json!({}))
            .unwrap();
        TaskContext {
            manager,
            paths,
            notifier: std::sync::Arc::new(NoopNotifier),
            task_id: record.id,
            profile_dir: None,
        }
    }

    fn make_plugin_source(name: &str, version: &str) -> PathBuf {
        // Mix nanosecond timestamp with PID so back-to-back invocations
        // never collide on the same directory name. Seconds-resolution
        // timestamps were prone to races when cargo test ran two cases
        // within the same wall-clock second.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = env::temp_dir().join(format!(
            "dshboxd-ext-src-{name}-{version}-{pid}-{nanos}"
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({
            "name": name,
            "version": version,
            "description": "test plugin",
            "dsh": {
                "bundle": {
                    "patch": "[]",
                },
            },
        });
        fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        dir
    }

    // Importing the same plugin twice should reuse the entry id from the
    // first call rather than minting a second `img-<task_id>` row.
    #[test]
    fn import_dedup_by_name_and_version() {
        // Acquire the daemon-wide test lock so we don't race with other
        // tests that mutate HOME (e.g. dispatch::tests::describe_container_*);
        // otherwise the config file is sometimes gone before the daemon's
        // `read_config` looks at it.
        let _lock = crate::test_support::env_lock();
        let home = sandbox("dedup-home");
        let runtime = sandbox("dedup-runtime");
        write_config(&home, &runtime);
        // `box_foundation::config_path` honours `DSHBOX_CONFIG_DIR`; this
        // is what production-like reads see on Windows where `HOME` is
        // ignored by `dirs::home_dir()`. The HOME line is kept for
        // compatibility with Linux/macOS where `dirs` falls back to it.
        let _guard = EnvGuard::set("HOME", &home);
        let _dshbox_dir = EnvGuard::set("DSHBOX_CONFIG_DIR", &home.join(".dsh-box"));
        // Repository imports now produce an immutable `npm pack` artifact.
        // Production initializes this once during daemon startup; unit tests
        // exercise the same path explicitly (another concurrent test may
        // have initialized it already, which is equally valid).
        let _ = crate::state::initialize_bundled_runtime();

        let source_a = make_plugin_source("dsh-better-sidebar", "0.12.3");
        let task = test_task(&runtime);
        let first = import_into_repository(&task, &source_a).unwrap();
        let source_b = make_plugin_source("dsh-better-sidebar", "0.12.3");
        let second = import_into_repository(&task, &source_b).unwrap();
        assert_eq!(
            first.id, second.id,
            "second import should reuse the cached entry"
        );
        let entries = scan_repository(&runtime);
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.name == "dsh-better-sidebar" && entry.version.as_deref() == Some("0.12.3")
            })
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "exactly one entry should match the package identity"
        );

        // Different version skews the cache key.
        let source_c = make_plugin_source("dsh-better-sidebar", "0.12.4");
        let task_v2 = test_task(&runtime);
        let third = import_into_repository(&task_v2, &source_c).unwrap();
        assert_ne!(
            first.id, third.id,
            "different version must create a new entry"
        );
        let entries = scan_repository(&runtime);
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| entry.name == "dsh-better-sidebar")
            .collect();
        assert_eq!(matching.len(), 2, "two distinct versions should coexist");

        let _ = fs::remove_dir_all(&source_a);
        let _ = fs::remove_dir_all(&source_b);
        let _ = fs::remove_dir_all(&source_c);
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests in this module run on a single thread by default
            // (no other tests touch HOME concurrently), and we restore the
            // previous value on drop.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { env::set_var(self.key, value) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn plugin_artifact_lives_next_to_the_repository_source() {
        let entry = RepositoryExtension {
            id: "img-1".to_owned(),
            kind: ExtensionKind::Plugin,
            name: "@example/plugin".to_owned(),
            version: Some("1.0.0".to_owned()),
            description: None,
            content_digest: "digest".to_owned(),
            source_path: "D:/runtime/repository/plugins/img-1/source".to_owned(),
            imported_at: 0,
            diagnostic: None,
            source: None,
        };
        assert_eq!(
            repository_plugin_artifact(&entry).unwrap(),
            PathBuf::from("D:/runtime/repository/plugins/img-1/artifact.tgz")
        );
    }
}
