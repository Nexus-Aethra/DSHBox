//! Unified template install/uninstall core.
//!
//! Every DSH Box entry point that wants to install, uninstall, or list
//! templates goes through this crate:
//!
//! - `dshboxd` daemon — wraps each call in a `box-scheduler` task so the
//!   HTTP RPC returns immediately and progress is broadcast on `/events`.
//! - `dshbox` CLI — invokes the same functions directly so `dshbox template
//!   install <ref>` works without a running daemon (provided a
//!   `runtime_directory` is configured).
//! - `dshbox` desktop — calls these via the daemon RPC; the desktop never
//!   touches the resource map directly.
//!
//! The split exists so the UI cannot drift from the CLI: both surfaces go
//! through the same code paths, and the IPC DTOs in `box-api` are the only
//! thing the UI sees.

use box_data_scheduler::{
    enqueue_for_hard_delete, read_resource_map, remove_resource, write_resource_map,
    ResourceEntry, ResourceStatus, ResourceType,
};
use box_dsh_versions::{
    classify_kind, parse_template_ref, read_template_index, version_directory, write_template_index,
    TemplateKind,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Result of a successful install: the new `TemplateEntry`, the resolved
/// `kind` (`Root`/`Common`), the absolute runtime path the clone lives at
/// (only meaningful for `Root` — common templates only materialise a script
/// entry), and the version slug (the bit after the `:` in
/// `github.com/<owner>/<repo>:<version>`).
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub entry: box_dsh_versions::TemplateEntry,
    pub runtime_path: PathBuf,
    pub version: String,
}

/// Install (or reinstall) a template by `ref_value`. The flow:
///
/// 1. `parse_template_ref` produces `(url, tag, version)` from
///    `github.com/<owner>/<repo>[:tag|@ref]`.
/// 2. `pull_template` clones the repo into
///    `<runtime>/runtimes/<version>/source/` (libgit2; no system git) and
///    writes the base `.dsh` script + manifest into the content-addressable
///    template store. We then call `write_template_with_entry` to record
///    the index entry, passing `kind = classify_kind(ref_value)` so root
///    templates get the optimised preparation path.
/// 3. **Root optimisation**: write a `.dsh-prepared` marker next to the
///    clone with `{ installedAt, commit, hasNodeModules }`. Future DSH
///    host launches read this marker and skip `pnpm install` when the
///    marker is fresh.
/// 4. Register the template in `<runtime>/state/resource-map.json` as a
///    `Template` row so the data-scheduler can soft-delete it later. The
///    `path` field points at the runtime directory (root) or the
///    template index (common); both are stable per-entry identifiers.
///
/// `cancelled` is polled between heavy phases; the daemon wires this to
/// the task cancel flag, the CLI ignores it (a one-shot CLI run cannot
/// abort mid-clone).
pub fn install_template<F: Fn() -> bool + Send + 'static>(
    runtime: &str,
    ref_value: &str,
    cancelled: F,
) -> Result<InstallOutcome, String> {
    let parsed = parse_template_ref(ref_value)
        .map_err(|error| format!("invalid template reference: {error}"))?;
    let kind = classify_kind(ref_value);

    // 1. Clone + write base template + index entry.
    let version = box_dsh_versions::pull_template(runtime, ref_value, cancelled)?;

    // 2. Re-read the index entry `pull_template` just wrote so we can hand
    //    the caller a structured `InstallOutcome` without re-parsing the
    //    index file ourselves.
    let index = read_template_index(runtime);
    let entry = index
        .values()
        .find(|e| e.name == ref_value || e.name == format!("{ref_value}:latest"))
        .cloned()
        .ok_or_else(|| {
            format!(
                "template pull succeeded but index entry is missing for `{ref_value}`"
            )
        })?;

    // 3. Root-only marker: the DSH host startup reads this to skip
    //    `pnpm install` on subsequent container boots when the harness
    //    is already warmed up.
    let runtime_path = version_directory(runtime, &version);
    if kind == TemplateKind::Root {
        write_root_prepared_marker(&runtime_path, &version, &parsed.tag.clone().unwrap_or_else(|| "latest".to_owned()))?;
    }

    // 4. Register in the data-scheduler resource map so uninstall can do
    //    the soft-delete + background hard-delete via the existing
    //    deletion-queue worker.
    let resource_path = if kind == TemplateKind::Root {
        runtime_path.to_string_lossy().into_owned()
    } else {
        // For common templates the on-disk artefact is the
        // content-addressable hash directory under `<runtime>/templates/<id>/`.
        box_dsh_versions::template_storage_root(runtime)
            .join(&entry.id)
            .to_string_lossy()
            .into_owned()
    };
    let mut entry_meta = std::collections::BTreeMap::new();
    entry_meta.insert("harness_ref".to_owned(), entry.harness_ref.clone().unwrap_or_default());
    entry_meta.insert("from_ref".to_owned(), entry.from_ref.clone().unwrap_or_default());
    let resource_id = box_data_scheduler::build_resource_id(
        ResourceType::Template,
        &entry.name,
        entry.harness_ref.as_deref(),
    );
    let resource_entry = ResourceEntry::new(resource_id.clone(), ResourceType::Template, resource_path);
    // Recover from a prior Deleted entry (reinstall after uninstall) so
    // the scheduler does not block us on the soft-delete fence.
    let mut map = read_resource_map(Path::new(runtime));
    if let Some(existing) = map.get(&entry.name) {
        if existing.status == ResourceStatus::Deleted {
            map.insert(
                entry.name.clone(),
                ResourceEntry {
                    status: ResourceStatus::Active,
                    ..resource_entry
                },
            );
            let _ = write_resource_map(Path::new(runtime), &map);
        } else {
            // Update path in case the directory was moved.
            map.insert(entry.name.clone(), resource_entry);
            let _ = write_resource_map(Path::new(runtime), &map);
        }
    } else {
        map.insert(entry.name.clone(), resource_entry);
        let _ = write_resource_map(Path::new(runtime), &map);
    }

    Ok(InstallOutcome {
        entry,
        runtime_path,
        version,
    })
}

/// Soft-delete a template by `name`. The contract:
///
/// - **Fast path** (this function): rename the runtime clone to
///   `<name>.deleted-<ts>`, drop the template-index entry, drop the
///   resource-map entry, enqueue a hard-delete in the data-scheduler's
///   fast queue, clear `selected_dsh_version` if it pointed here.
/// - **Background**: the data-scheduler's `DeletionWorker` drains the
///   fast queue every tick and removes the renamed directory via
///   `remove_dir_all`. Failures are retried by the slow queue (60s
///   interval, 5 attempts) before becoming `permanent_failures`.
///
/// Returns the deleted `(id, path)` pair so the caller can surface a
/// diagnostic ("scheduled for deletion at <path>") if useful.
pub fn uninstall_template(runtime: &str, name: &str) -> Result<(String, String), String> {
    let index = read_template_index(runtime);
    let entry = index
        .get(name)
        .cloned()
        .ok_or_else(|| format!("template `{name}` is not installed"))?;
    let version = entry
        .harness_ref
        .clone()
        .ok_or_else(|| format!("template `{name}` has no harness ref; cannot derive runtime path"))?;

    // Soft-delete: rename the runtime directory if it exists. The rename
    // itself is atomic on Windows when the destination lives on the same
    // volume; the subsequent remove_dir_all is what the data-scheduler
    // runs in the background.
    let runtime_path = version_directory(runtime, &version);
    if runtime_path.exists() {
        let ts = box_foundation::now_seconds();
        let archived = runtime_path
            .with_file_name(format!(
                "{}.deleted-{}",
                runtime_path.file_name().and_then(|s| s.to_str()).unwrap_or("template"),
                ts
            ));
        // The runtime dir is `<root>/runtimes/<version>/source`; archive
        // the parent `<root>/runtimes/<version>` so the `source` subtree
        // goes with it.
        let archived_dir = archived.parent().map(Path::to_path_buf).unwrap_or(archived.clone());
        if let Some(parent) = runtime_path.parent() {
            if parent.exists() {
                let renamed = parent.with_file_name(format!(
                    "{}.deleted-{}",
                    parent.file_name().and_then(|s| s.to_str()).unwrap_or("template"),
                    ts
                ));
                let _ = fs::rename(parent, &renamed);
            }
        }
        // Tell the data-scheduler where the renamed directory lives so
        // the background worker can `remove_dir_all` it.
        let resource_path = archived_dir.to_string_lossy().into_owned();
        enqueue_for_hard_delete(Path::new(runtime), &format!("template:{name}"), &resource_path)?;
    }

    // Drop the resource-map entry (soft-delete: mark Deleted if we still
    // need to keep it around, otherwise remove the row entirely).
    let map_path = Path::new(runtime);
    let _ = remove_resource(map_path, name);

    // Drop the template-index entry.
    let mut index = read_template_index(runtime);
    index.remove(name);
    write_template_index(runtime, &index)?;

    // Clear `selected_dsh_version` if it pointed at this template's version.
    let Ok(mut config) = box_foundation::read_config() else {
        return Ok((format!("template:{name}"), String::new()));
    };
    if config.selected_dsh_version.as_deref() == Some(&version)
        || config.selected_dsh_version.as_deref() == Some(name)
    {
        config.selected_dsh_version = None;
        let _ = box_foundation::write_config(&config);
    }

    Ok((format!("template:{name}"), String::new()))
}

/// Write the `.dsh-prepared` marker that tells DSH host startup to skip
/// `pnpm install`. The marker is intentionally JSON so future fields
/// (e.g. `committedAt`) can be added without breaking older builds.
fn write_root_prepared_marker(
    runtime_path: &Path,
    version: &str,
    harness_tag: &str,
) -> Result<(), String> {
    let marker = serde_json::json!({
        "version": version,
        "harnessTag": harness_tag,
        "preparedAt": box_foundation::now_seconds(),
        "hasNodeModules": false,
    });
    fs::write(
        runtime_path.join(".dsh-prepared"),
        serde_json::to_string_pretty(&marker).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}