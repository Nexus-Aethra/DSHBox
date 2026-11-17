//! The prepared-base index: which DSH version has a prepared tree on this
//! machine, and where that tree lives.
//!
//! Pure storage plus one legacy migration. It never touches the network and never
//! runs pnpm -- those belong to dsh_catalog and to the pull orchestration in
//! mod.rs.

use super::*;

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    box_foundation::is_safe_identifier(version)
}

#[derive(Deserialize)]
pub(super) struct GitHubTag {
    pub(super) name: String,
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

pub(super) type PreparedBaseIndex = BTreeMap<String, PreparedBaseRecord>;

fn prepared_base_index_path(layout: &RuntimeLayout) -> PathBuf {
    layout.state_dir().join("prepared-bases.json")
}

pub(super) fn read_prepared_base_index(layout: &RuntimeLayout) -> Result<PreparedBaseIndex, String> {
    match fs::read_to_string(prepared_base_index_path(layout)) {
        Ok(body) => serde_json::from_str(&body)
            .map_err(|error| format!("cannot parse prepared-base index: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(format!("cannot read prepared-base index: {error}")),
    }
}

pub(super) fn write_prepared_base_index(
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

/// Return every prepared Harness base that can be used as a direct container
/// template. This is deliberately shared with the template listing so the
/// CLI and desktop never disagree about which roots are runnable.
pub(crate) fn list_prepared_bases(root: &str) -> Result<Vec<PreparedBaseRecord>, String> {
    let layout = RuntimeLayout::new(root);
    layout.initialize_schema_10()?;
    Ok(read_prepared_base_index(&layout)?.into_values().collect())
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
#[allow(
    dead_code,
    reason = "legacy migration is intentionally not run during rapid schema iteration"
)]
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
