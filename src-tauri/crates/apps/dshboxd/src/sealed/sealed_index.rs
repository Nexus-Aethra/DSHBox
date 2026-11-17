//! The sealed-template index: which sealed templates exist, and the record
//! each one carries.
//!
//! Pure storage. Reading and writing the index is all it does; building a
//! template is mod.rs is the orchestrator's job.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SealedTemplateRecord {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) base_id: String,
    pub(crate) profile: String,
    pub(crate) directory: String,
    #[serde(default)]
    pub(crate) plugin_artifact_ids: Vec<String>,
    #[serde(default)]
    pub(crate) plugin_sources: Vec<String>,
    pub(crate) created_at: u64,
    pub(crate) size_bytes: u64,
}

type SealedTemplateIndex = BTreeMap<String, SealedTemplateRecord>;

fn index_path(layout: &RuntimeLayout) -> PathBuf {
    layout.state_dir().join("sealed-templates.json")
}

pub(super) fn read_index(layout: &RuntimeLayout) -> Result<SealedTemplateIndex, String> {
    match fs::read_to_string(index_path(layout)) {
        Ok(body) => serde_json::from_str(&body)
            .map_err(|error| format!("cannot parse sealed-template index: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(format!("cannot read sealed-template index: {error}")),
    }
}

pub(super) fn write_index(layout: &RuntimeLayout, index: &SealedTemplateIndex) -> Result<(), String> {
    atomic_write_json(&index_path(layout), index)
}


/// Schema-10 templates carry no mutable data snapshots. Keep the RPC as a
/// harmless compatibility operation while old snapshot pruning is removed.
pub(crate) fn prune_sealed_template_snapshots() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

pub(crate) fn sealed_template_by_name(
    root: &str,
    name: &str,
) -> Result<SealedTemplateRecord, String> {
    let layout = RuntimeLayout::new(root);
    layout.initialize_schema_10()?;
    read_index(&layout)?
        .remove(name)
        .ok_or_else(|| format!("sealed template `{name}` not found"))
}
