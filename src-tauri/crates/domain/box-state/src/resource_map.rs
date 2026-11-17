//! `resource-map.json` — the single source of truth for all managed resources.

use box_foundation::{now_seconds, BoxResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Runtime-relative path of the resource map.
pub fn resource_map_path(runtime: &Path) -> PathBuf {
    runtime.join("state").join("resource-map.json")
}

/// Stable cross-reference identifier: `"<type>:<hash>"`.
pub fn build_resource_id(resource_type: ResourceType, name: &str, version: Option<&str>) -> String {
    let raw = match version {
        Some(v) if !v.is_empty() => format!("{name}:{v}"),
        _ => name.to_owned(),
    };
    format!("{}:{}", resource_type.as_str(), fnv1a64_hex(&raw))
}

/// Determine the resource type prefix from an id string.
pub fn resource_type_from_id(id: &str) -> ResourceType {
    id.split(':').next().map(ResourceType::from_str).unwrap_or(ResourceType::Unknown)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    #[default]
    Plugin,
    Template,
    Container,
    Unknown,
}

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Template => "template",
            Self::Container => "container",
            Self::Unknown => "unknown",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "plugin" => Self::Plugin,
            "template" => Self::Template,
            "container" => Self::Container,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceStatus {
    #[default]
    Active,
    Deleted,
}

/// One row in the resource map. `id` is the stable cross-reference handle;
/// the map key itself is the human-readable name (`"name:version"` or the
/// container timestamp). `refs` lists `resource_id`s that still hold a
/// reference to this entry — deletion is refused while refs is non-empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceEntry {
    pub id: String,
    pub r#type: ResourceType,
    pub path: String,
    pub status: ResourceStatus,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
    pub created_at: u64,
}

impl ResourceEntry {
    pub fn new(id: String, r#type: ResourceType, path: String) -> Self {
        Self {
            id,
            r#type,
            path,
            status: ResourceStatus::Active,
            refs: Vec::new(),
            meta: BTreeMap::new(),
            created_at: now_seconds(),
        }
    }
    pub fn has_refs(&self) -> bool {
        !self.refs.is_empty()
    }
}

/// Read the resource map from disk. Missing or malformed files read as empty.
pub fn read_resource_map(runtime: &Path) -> BTreeMap<String, ResourceEntry> {
    let path = resource_map_path(runtime);
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the resource map with a temp-file-then-rename strategy so a crash
/// mid-write never leaves a torn JSON file.
pub fn write_resource_map(
    runtime: &Path,
    entries: &BTreeMap<String, ResourceEntry>,
) -> BoxResult<()> {
    let path = resource_map_path(runtime);
    write_json_atomic(&path, entries)
}

/// Add or replace a resource entry. Returns `Ok(true)` when the entry was
/// newly inserted, `Ok(false)` when it replaced an existing active entry.
/// Refusing to overwrite a `Deleted` entry — the caller should call
/// `enqueue_for_hard_delete` first or recover it explicitly.
pub fn add_resource(
    runtime: &Path,
    name: &str,
    entry: ResourceEntry,
) -> BoxResult<bool> {
    let mut entries = read_resource_map(runtime);
    let existed = entries.contains_key(name);
    if let Some(existing) = entries.get(name) {
        if existing.status == ResourceStatus::Deleted {
            return Err(format!("resource '{}' is marked deleted; recover it first", name));
        }
    }
    entries.insert(name.to_owned(), entry);
    write_resource_map(runtime, &entries)?;
    Ok(!existed)
}

/// Soft-delete: remove the active entry and return its path for queue
/// enqueuing. Refused when `refs` is non-empty — the caller must detach
/// references first. Returns the (id, path) pair on success.
pub fn remove_resource(runtime: &Path, name: &str) -> BoxResult<(String, String)> {
    let mut entries = read_resource_map(runtime);
    let entry = entries
        .remove(name)
        .ok_or(format!("resource '{}' not found in map", name))?;
    if entry.has_refs() {
        let holders = entry.refs.join(", ");
        return Err(format!(
            "resource '{}' is still referenced by: {}",
            name, holders
        ));
    }
    write_resource_map(runtime, &entries)?;
    Ok((entry.id, entry.path))
}

/// Look up one entry by name.
pub fn get_resource(
    runtime: &Path,
    name: &str,
) -> BoxResult<ResourceEntry> {
    let entries = read_resource_map(runtime);
    entries
        .get(name)
        .cloned()
        .ok_or(format!("resource '{}' not found in map", name).into())
}

/// Check whether an active (non-deleted) entry exists for the given name.
pub fn has_active_resource(runtime: &Path, name: &str) -> bool {
    matches!(get_resource(runtime, name), Ok(e) if e.status == ResourceStatus::Active)
}

/// Recover a previously soft-deleted resource by name. The entry must be
/// absent from the map (already removed by `remove_resource`); this
/// re-inserts it as active. Used when a user undoes a delete or when a
/// reinstall needs to reuse the existing on-disk directory.
pub fn recover_resource(
    runtime: &Path,
    name: &str,
    entry: ResourceEntry,
) -> BoxResult<bool> {
    let mut entries = read_resource_map(runtime);
    if let Some(existing) = entries.get(name) {
        if existing.status == ResourceStatus::Active {
            return Err(format!("resource '{}' is already active", name));
        }
    }
    let existed = entries.contains_key(name);
    let mut entry = entry;
    entry.status = ResourceStatus::Active;
    entries.insert(name.to_owned(), entry);
    write_resource_map(runtime, &entries)?;
    Ok(!existed)
}

/// Add a cross-resource reference: `owner_id` now references `resource_name`.
/// Idempotent — adding a ref that is already present is a no-op.
pub fn add_reference(
    runtime: &Path,
    resource_name: &str,
    owner_id: &str,
) -> BoxResult<()> {
    let mut entries = read_resource_map(runtime);
    let entry = entries
        .entry(resource_name.to_owned())
        .or_insert_with(|| ResourceEntry::new(
            format!("unknown:{resource_name}"),
            ResourceType::Unknown,
            String::new(),
        ));
    if !entry.refs.contains(&owner_id.to_owned()) {
        entry.refs.push(owner_id.to_owned());
    }
    write_resource_map(runtime, &entries)
}

/// Remove a cross-resource reference. If the entry ends up with zero refs
/// and is already deleted, it is kept on the map as a tombstone for the
/// deletion queue to find.
pub fn remove_reference(
    runtime: &Path,
    resource_name: &str,
    owner_id: &str,
) -> BoxResult<()> {
    let mut entries = read_resource_map(runtime);
    let entry = entries
        .get_mut(resource_name)
        .ok_or(format!(
            "resource '{}' not found for reference removal",
            resource_name
        ))?;
    entry.refs.retain(|id| id != owner_id);
    write_resource_map(runtime, &entries)
}

/// List all resource ids whose refs are empty (deletion candidates).
pub fn resources_without_refs(runtime: &Path) -> Vec<String> {
    read_resource_map(runtime)
        .into_iter()
        .filter(|(_, entry)| entry.status == ResourceStatus::Active && entry.refs.is_empty())
        .map(|(name, _)| name)
        .collect()
}

/// Write JSON to `<path>` via a `.tmp` file then `rename` so a crash
/// mid-write never leaves a torn file. Matches the crate's atomic-replace
/// guarantee. The tmp file is placed next to the target so `rename` is
/// same-directory (always atomic on POSIX and Windows NTFS).
pub(crate) fn write_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
) -> BoxResult<()> {
    let parent = path.parent().ok_or("target path has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|error| error.to_string())?;
    let serialized = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp_name = format!("{}.json.tmp", stem);
    let tmp = parent.join(&tmp_name);
    fs::write(&tmp, serialized)
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    fs::rename(&tmp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

/// FNV-1a 64-bit hash of a UTF-8 string, formatted as 16-char zero-padded hex.
/// Same algorithm as `box_extensions::extension_digest` (sans the `fnv1a64:` prefix).
pub fn fnv1a64_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fresh_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dshbox-map-test-{}-{}",
            now_seconds(),
            uuid::Uuid::new_v4().simple().to_string(),
        ))
    }

    fn mk_entry(name: &str, r#type: ResourceType) -> ResourceEntry {
        ResourceEntry::new(build_resource_id(r#type, name, None), r#type, format!("store/{name}"))
    }

    #[test]
    fn add_and_get_roundtrip() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let entry = mk_entry("demo", ResourceType::Plugin);
        assert!(add_resource(&root, "demo", entry).unwrap());
        let got = get_resource(&root, "demo").unwrap();
        // `demo` (no version) → `build_resource_id` uses `plugin:` +
        // `fnv1a64_hex("demo")` — the hash is deterministic.
        assert_eq!(got.id, format!("plugin:{}", fnv1a64_hex("demo")));
        assert_eq!(got.status, ResourceStatus::Active);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fnv1a64_hex_is_deterministic_and_stable() {
        let hash_demo = fnv1a64_hex("demo");
        let hash_tag = fnv1a64_hex("dsh-v0.1.0-rc.7");
        // Sanity: non-empty string produces a 16-char hex digest.
        assert_eq!(hash_demo.len(), 16);
        assert!(hash_demo.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash_tag.len(), 16);
        // Empty string → FNV-1a offset basis (the seed, since no bytes
        // are XOR'd or multiplied).
        assert_eq!(fnv1a64_hex(""), "cbf29ce484222325");
        // Same input → same output.
        assert_eq!(fnv1a64_hex("a:b"), fnv1a64_hex("a:b"));
        // Different input → different output.
        assert_ne!(fnv1a64_hex("a:b"), fnv1a64_hex("a:c"));
    }

    #[test]
    fn remove_refused_when_refs_present() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "locked", mk_entry("locked", ResourceType::Plugin)).unwrap();
        add_reference(&root, "locked", "container:123").unwrap();
        let result = remove_resource(&root, "locked");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("still referenced"));
        // Entry still active after failed remove.
        assert!(has_active_resource(&root, "locked"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_then_recover() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "x", mk_entry("x", ResourceType::Template)).unwrap();

        let (id, _path) = remove_resource(&root, "x").unwrap();
        assert!(!has_active_resource(&root, "x"));

        // Enqueue for hard delete (simulated — queue is separate module).
        // Recover the resource.
        let recovered = mk_entry("x", ResourceType::Template);
        let recovered = {
            let mut e = recovered;
            e.id = id;
            e
        };
        recover_resource(&root, "x", recovered).unwrap();
        assert!(has_active_resource(&root, "x"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn add_ref_and_remove_ref() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "res", mk_entry("res", ResourceType::Plugin)).unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 0);

        add_reference(&root, "res", "container:a").unwrap();
        add_reference(&root, "res", "container:a").unwrap(); // idempotent
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 1);

        add_reference(&root, "res", "container:b").unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 2);

        remove_reference(&root, "res", "container:a").unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        assert!(read_resource_map(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
