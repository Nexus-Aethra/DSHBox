//! DSH version management for the daemon: catalog refresh, install, and
//! uninstall. Mirrors the desktop's `versions.rs` + `commands/versions.rs`
//! without any Tauri dependency.
//!
//! Templates are the single source of truth: harness clones are mirrored
//! into the template index by `pull_template`, and the Harness tab in the
//! UI is a derived view over that index (no separate `dsh-catalog.json`
//! or legacy "base template" writer any more). The remote tag list is
//! fetched on demand for the "Load versions" button — it is never
//! persisted.

use box_dsh_versions::{
    installed_versions, parse_template_ref, read_template_index, DshVersion, HARNESS_STANDARD_REF,
};
use box_foundation::{
    atomic_write_json, mirror_url, now_seconds, read_config, write_config, write_template_manifest,
    PublishedResourceKind, RuntimeLayout, TemplateManifest, STORAGE_SCHEMA_VERSION,
};
use box_runtime::{
    process::{ExecutionKind, ProcessSpec},
    shallow_clone_with_cancel,
};
use box_scheduler::TaskContext;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::info;

use crate::{
    state::bundled_runtime,
    toolchains::{pnpm_policy, resolve_toolchain, run_logged, TaskCancel},
};

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    box_foundation::is_safe_identifier(version)
}

#[derive(Deserialize)]
struct GitHubTag {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedBaseRecord {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) source_ref: String,
    pub(crate) source_commit: String,
    pub(crate) directory: String,
    pub(crate) created_at: u64,
    pub(crate) size_bytes: u64,
}

type PreparedBaseIndex = BTreeMap<String, PreparedBaseRecord>;

fn prepared_base_index_path(layout: &RuntimeLayout) -> PathBuf {
    layout.state_dir().join("prepared-bases.json")
}

fn read_prepared_base_index(layout: &RuntimeLayout) -> Result<PreparedBaseIndex, String> {
    match fs::read_to_string(prepared_base_index_path(layout)) {
        Ok(body) => serde_json::from_str(&body)
            .map_err(|error| format!("cannot parse prepared-base index: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(format!("cannot read prepared-base index: {error}")),
    }
}

fn write_prepared_base_index(
    layout: &RuntimeLayout,
    index: &PreparedBaseIndex,
) -> Result<(), String> {
    atomic_write_json(&prepared_base_index_path(layout), index)
}

pub(crate) fn prepared_base_for_version(
    root: &str,
    version: &str,
) -> Result<PreparedBaseRecord, String> {
    let layout = RuntimeLayout::new(root);
    layout.initialize_schema_10()?;
    read_prepared_base_index(&layout)?
        .remove(version)
        .ok_or_else(|| format!("prepared Harness base `{version}` is not installed"))
}

/// Resolve the canonical FROM reference into its mirror-aware clone URL.
/// Centralised here so the CLI and the desktop don't reach into
/// `box_dsh_versions` themselves.
fn mirror_url_for(ref_value: &str) -> Result<String, String> {
    let parsed = parse_template_ref(ref_value)
        .map_err(|error| format!("invalid harness reference: {error}"))?;
    let mirror = read_config().ok().and_then(|config| config.github_mirror);
    Ok(mirror_url(&parsed.url, mirror.as_deref()))
}

/// Fetch the harness version catalog. Primary path is the git protocol via
/// libgit2 (`ls-remote` equivalent, no system git executable, no GitHub API
/// rate limits): the harness repo's tags ARE the version list. The GitHub
/// REST API stays as a fallback for networks where the git transport is
/// blocked but plain HTTPS works.
///
/// The returned list is **never** persisted — it is shown only when the
/// user explicitly asks to "Load versions", so a one-off network outage
/// does not produce a stale catalog on disk.
pub(crate) fn fetch_remote_dsh_tags() -> Result<Vec<String>, String> {
    let config = read_config()?;
    let target = mirror_url_for(HARNESS_STANDARD_REF)?;
    match box_runtime::list_remote_tags(&target) {
        Ok(tags) => {
            let filtered: Vec<String> = tags
                .into_iter()
                .filter(|name| is_safe_version_name(name))
                .collect();
            if !filtered.is_empty() {
                return Ok(filtered);
            }
            // Repo reachable but tagless: fall through to the API so a
            // legitimately empty ls-remote does not blank the catalog.
        }
        Err(error) => {
            eprintln!("dshboxd: git tag listing failed ({error}); falling back to the GitHub API");
        }
    }
    fetch_remote_dsh_tags_api(config.github_mirror.as_deref())
}

fn fetch_remote_dsh_tags_api(github_mirror: Option<&str>) -> Result<Vec<String>, String> {
    use box_dsh_versions::DSH_TAGS_API;
    let endpoint = mirror_url(DSH_TAGS_API, github_mirror);
    let client = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("cannot create GitHub client: {error}"))?;
    let response = client
        .get(&endpoint)
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let tags: Vec<GitHubTag> = response
        .error_for_status()
        .map_err(|error| format!("GitHub tags request failed: {error}"))?
        .json()
        .map_err(|error| format!("cannot parse GitHub tags: {error}"))?;
    Ok(tags
        .into_iter()
        .map(|tag| tag.name)
        .filter(|name| is_safe_version_name(name))
        .collect())
}

/// In-memory refresh kept around as a thin wrapper so the existing
/// `dsh-catalog-refresh` task semantics do not change for the UI. It used
/// to write `state/dsh-catalog.json`; now it just touches the network
/// and prints the result (no persistence, no IPC return value of
/// consequence — the UI re-queries `list_dsh_catalog` for the merged
/// list right after the task settles).
pub(crate) fn refresh_dsh_catalog() -> Result<(), String> {
    fetch_remote_dsh_tags().map(|_| ())
}

/// Pull and prepare a root Harness template. The base seeds the bundled pnpm
/// cache but deliberately does not build the frontend: the final build must
/// happen after a container has added its local plugin artifacts.
pub(crate) fn pull_template_with_cancel(
    ref_value: String,
    cancelled: impl Fn() -> bool + Send + 'static,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .clone()
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let parsed = parse_template_ref(&ref_value)
        .map_err(|error| format!("invalid harness reference: {error}"))?;
    let mut index = read_prepared_base_index(&layout)?;
    if index.contains_key(&parsed.version) {
        return Err(format!(
            "prepared Harness base already exists for {}",
            parsed.version
        ));
    }

    task.update("Cloning Harness source", 10);
    task.log(&format!("cloning {ref_value} into private staging"));
    let staged = layout.create_staging_dir("template_pull")?;
    let harness = staged.join("harness");
    let prepare = (|| -> Result<PreparedBaseRecord, String> {
        let mirror = read_config().ok().and_then(|config| config.github_mirror);
        let target = mirror_url(&parsed.url, mirror.as_deref());
        let revision = parsed.tag.as_deref().filter(|tag| *tag != "latest");
        let commit = shallow_clone_with_cancel(&target, &harness, revision, cancelled)
            .map_err(|error| {
                if error.contains("SSL") {
                    format!(
                        "pull template failed: {error}\nhint: github.com may be blocked on this network; configure a mirror with `dshbox config set mirror <prefix>` (e.g. https://gh-proxy.com)"
                    )
                } else {
                    format!("pull template failed: {error}")
                }
            })?;
        task.check_cancelled()?;

        let pnpm = resolve_toolchain("pnpm")?;
        let log_path = staged.join("prepare.log");
        task.update("Installing DSH dependencies", 35);
        task.log("installing DSH dependencies in prepared-base staging");
        run_pnpm_task(
            &pnpm,
            &harness,
            ["install"],
            &log_path,
            task,
            "prepared-base dependency install",
        )?;
        task.check_cancelled()?;

        task.update("Validating prepared base", 60);
        validate_prepared_harness(&harness)?;
        let runtime = bundled_runtime()?;
        let created_at = now_seconds();
        let digest = box_dsh_versions::template_content_hash(&format!(
            "{}\n{}\n{}\n{}",
            ref_value, commit, runtime.node_version, runtime.pnpm_version
        ));
        let id = format!("base-{digest}");
        Ok(PreparedBaseRecord {
            id,
            version: parsed.version.clone(),
            source_ref: ref_value.clone(),
            source_commit: commit,
            directory: String::new(),
            created_at,
            size_bytes: 0,
        })
    })();

    let mut record = match prepare {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
    };
    // The prepared base is copied later as source only. Its staging-local pnpm
    // links are never followed or launched, so relinking them after publish
    // would only re-run the Windows junction failure we are avoiding.
    record.size_bytes = tree_size_bytes(&staged)?;
    let runtime = bundled_runtime()?;
    let manifest = TemplateManifest {
        schema_version: STORAGE_SCHEMA_VERSION,
        kind: PublishedResourceKind::PreparedBase,
        id: record.id.clone(),
        source_ref: record.source_ref.clone(),
        source_commit: record.source_commit.clone(),
        node_version: runtime.node_version.clone(),
        pnpm_version: runtime.pnpm_version.clone(),
        base_id: None,
        plugin_artifact_ids: Vec::new(),
        plugin_sources: Vec::new(),
        harness_digest: record
            .id
            .strip_prefix("base-")
            .unwrap_or(&record.id)
            .to_owned(),
        profile_digest: None,
        size_bytes: record.size_bytes,
        validated_at: now_seconds(),
    };
    write_template_manifest(&staged, &manifest)?;
    task.update("Publishing prepared base", 84);
    let destination =
        layout.prepared_base_dir(record.id.strip_prefix("base-").unwrap_or(&record.id))?;
    if destination.exists() {
        let _ = fs::remove_dir_all(&staged);
        return Err(format!(
            "prepared base already exists at {}",
            destination.display()
        ));
    }
    layout.publish_staged_tree(&staged, &destination)?;
    record.directory = destination.to_string_lossy().into_owned();
    index.insert(record.version.clone(), record.clone());
    write_prepared_base_index(&layout, &index)?;
    let mut updated = read_config()?;
    updated.selected_dsh_version = Some(record.version.clone());
    write_config(&updated)?;
    task.update("Prepared base ready", 100);
    task.log(&format!(
        "prepared base {} ({}) is ready",
        record.version, record.id
    ));
    info!(
        "prepared Harness base {} (resolved version {})",
        ref_value, record.version
    );
    Ok(())
}

fn run_pnpm_task<const N: usize>(
    pnpm: &crate::toolchains::ResolvedToolchain,
    directory: &Path,
    args: [&str; N],
    log_path: &Path,
    task: &TaskContext,
    label: &str,
) -> Result<(), String> {
    let directory_arg = directory.to_string_lossy().into_owned();
    let spec = ProcessSpec::new(&pnpm.path)
        .args(&pnpm.arguments)
        .args(["--dir", directory_arg.as_str()])
        .args(args)
        .cwd(directory)
        .policy(pnpm_policy(pnpm))
        .kind(ExecutionKind::Logged)
        .log_path(log_path);
    let mut process = run_logged(&spec, label)?;
    let status = process
        .wait_or_kill(&TaskCancel(Some(task)), Duration::from_secs(900), label)
        .map_err(|error| format!("{label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} failed; inspect {}", log_path.display()))
    }
}

fn validate_prepared_harness(harness: &Path) -> Result<(), String> {
    for relative in [
        "package.json",
        "node_modules/tsx/package.json",
        "apps/cli/src/bin.ts",
    ] {
        if !harness.join(relative).is_file() {
            return Err(format!(
                "prepared Harness validation failed: missing {relative}"
            ));
        }
    }
    Ok(())
}

fn tree_size_bytes(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            total = total.saturating_add(tree_size_bytes(&entry.path())?);
        } else if file_type.is_file() {
            total =
                total.saturating_add(entry.metadata().map_err(|error| error.to_string())?.len());
        }
    }
    Ok(total)
}

/// Uninstall a DSH harness: soft-delete the runtime clone, drop the
/// template index entry, and enqueue the background hard-delete via the
/// data-scheduler's deletion queue.
///
/// Delegates to `box_template_core::uninstall_template` so the daemon and
/// the CLI share the same soft-delete logic (rename + resource-map cleanup
/// + deletion-queue enqueue). The actual `remove_dir_all` happens in the
/// background deletion worker, not in this call — the function returns
/// within a few hundred milliseconds.
pub(crate) fn uninstall_dsh_version(version: &str) -> Result<(), String> {
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let mut index = read_prepared_base_index(&layout)?;
    let record = index
        .remove(version)
        .ok_or_else(|| format!("prepared Harness base `{version}` is not installed"))?;
    let directory = PathBuf::from(&record.directory);
    let templates = layout.templates_dir();
    if !directory.starts_with(&templates)
        || !directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with("base-"))
            .unwrap_or(false)
    {
        return Err("prepared-base index contains an unsafe directory".to_owned());
    }
    if directory.exists() {
        fs::remove_dir_all(&directory).map_err(|error| {
            format!(
                "cannot remove prepared base {}: {error}",
                directory.display()
            )
        })?;
    }
    write_prepared_base_index(&layout, &index)?;
    // Also clear the selected version if it pointed at this tag.
    let mut config = read_config()?;
    if config.selected_dsh_version.as_deref() == Some(version) {
        config.selected_dsh_version = None;
        let _ = write_config(&config);
    }
    Ok(())
}

/// Build the `DshVersion[]` payload the Harness tab shows. Every entry is
/// derived from the template index: name = `harness_ref`, `installed` =
/// `<runtime>/runtimes/<tag>/source/.dshbox-runtime.json` exists.
///
/// Remote tags fetched by `refresh_dsh_catalog` are merged in (always
/// marked `installed = false` unless the template index says otherwise)
/// so users still see new releases between pulls.
pub(crate) fn list_dsh_versions_derived(
    remote_tags: Option<Vec<String>>,
) -> Result<Vec<DshVersion>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;

    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let index = read_prepared_base_index(&layout)?;
    let mut by_name: std::collections::BTreeMap<String, DshVersion> =
        std::collections::BTreeMap::new();
    for record in index.values() {
        if !is_safe_version_name(&record.version) {
            continue;
        }
        let installed = PathBuf::from(&record.directory)
            .join("manifest.json")
            .is_file();
        by_name
            .entry(record.version.clone())
            .and_modify(|existing| existing.installed = existing.installed || installed)
            .or_insert(DshVersion {
                name: record.version.clone(),
                installed,
            });
    }

    // Remote-only entries (always uninstalled).
    if let Some(tags) = remote_tags {
        for tag in tags {
            if !is_safe_version_name(&tag) {
                continue;
            }
            by_name.entry(tag.clone()).or_insert(DshVersion {
                name: tag,
                installed: false,
            });
        }
    }

    // Make sure `latest` shows up even when nothing is installed yet —
    // it's the implicit default branch reference.
    by_name.entry("latest".to_owned()).or_insert(DshVersion {
        name: "latest".to_owned(),
        installed: false,
    });

    Ok(by_name.into_values().collect())
}

/// Just the `installed` subset — useful for the Container page badge.
pub(crate) fn list_installed_dsh_versions() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    Ok(read_prepared_base_index(&layout)?.into_keys().collect())
}

/// One-time migration for users that have runtime clones but no template
/// index entries (older installs that used the legacy `install_dsh_version`
/// path). For every `runtimes/<tag>/source/.dshbox-runtime.json` that does
/// not yet have a template index entry, register one with the canonical
/// `FROM github.com/deepseek-ai/deepseek-harness:<tag>` body.
///
/// Idempotent: a second call is a no-op once every runtime is mirrored.
/// Returns the tags that were registered in this pass so the caller can
/// surface a diagnostic message.
pub(crate) fn migrate_runtime_runtimes_to_templates() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let index = read_template_index(&root);
    let existing: std::collections::BTreeSet<String> = index
        .values()
        .filter_map(|entry| entry.harness_ref.clone())
        .collect();
    let mut registered = Vec::new();
    for tag in installed_versions(&root)? {
        if existing.contains(&tag) {
            continue;
        }
        let ref_value = format!("github.com/deepseek-ai/deepseek-harness:{tag}");
        let body = format!("FROM {ref_value}\nPROFILE web\nNAME {ref_value}\nVERSION latest\n");
        // Drop the body into the hash-addressed store, mirroring what
        // `pull_template` does. `write_template_with_entry` also updates
        // the manifest and index, so no extra bookkeeping is needed.
        // The kind is always `Root` here — this migration only handles
        // legacy harness clones, never user-authored templates.
        box_dsh_versions::write_template_with_entry(
            &root,
            &ref_value,
            &body,
            Some(tag.clone()),
            "web",
            Some(ref_value.clone()),
            now_seconds(),
            box_dsh_versions::TemplateKind::Root,
        )
        .map_err(|error| format!("cannot register migrated template for `{tag}`: {error}"))?;
        registered.push(tag);
    }
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_harness_validation_requires_dependency_cache_but_not_build_output() {
        let temporary = tempfile::tempdir().unwrap();
        let harness = temporary.path().join("harness");
        fs::create_dir_all(harness.join("node_modules/tsx")).unwrap();
        fs::create_dir_all(harness.join("apps/cli/src")).unwrap();
        fs::write(harness.join("package.json"), "{}").unwrap();
        fs::write(harness.join("apps/cli/src/bin.ts"), "").unwrap();
        assert!(validate_prepared_harness(&harness).is_err());
        fs::write(harness.join("node_modules/tsx/package.json"), "{}").unwrap();
        validate_prepared_harness(&harness).unwrap();
    }

    #[test]
    fn prepared_base_index_round_trips_without_runtime_paths_in_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temporary.path());
        layout.initialize_schema_10().unwrap();
        let mut index = PreparedBaseIndex::new();
        index.insert(
            "latest".to_owned(),
            PreparedBaseRecord {
                id: "base-abc".to_owned(),
                version: "latest".to_owned(),
                source_ref: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
                source_commit: "abc".to_owned(),
                directory: layout
                    .prepared_base_dir("abc")
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                created_at: 1,
                size_bytes: 2,
            },
        );
        write_prepared_base_index(&layout, &index).unwrap();
        assert_eq!(
            read_prepared_base_index(&layout)
                .unwrap()
                .get("latest")
                .unwrap()
                .id,
            "base-abc"
        );
    }
}
