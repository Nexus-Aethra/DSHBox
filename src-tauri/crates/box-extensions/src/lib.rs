//! Read-only discovery of per-container DSH profiles, plugins, and skills.

pub mod transfer;

use box_containers::DshContainer;
use box_foundation::now_seconds;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionKind {
    Plugin,
    Skill,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRecord {
    pub kind: ExtensionKind,
    pub name: String,
    pub source_kind: String,
    pub source: String,
    pub profile: Option<String>,
    pub path: String,
    pub installed_at: u64,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub content_digest: Option<String>,
}

/// One immutable extension source owned by the DSH Box repository.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryExtension {
    pub id: String,
    pub kind: ExtensionKind,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub content_digest: String,
    pub source_path: String,
    pub imported_at: u64,
    pub diagnostic: Option<String>,
    /// Original import source (GitHub URL, directory, or archive path). Quick
    /// bundle exports use this to keep GitHub entries as URLs instead of
    /// embedding their content.
    #[serde(default)]
    pub source: Option<String>,
}

/// A valid extension candidate found inside one Container workspace.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceExtension {
    pub kind: ExtensionKind,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub relative_path: String,
    pub content_digest: String,
    pub diagnostic: Option<String>,
}

/// Finds extension roots in a workspace without following symlinks or entering dependency output.
pub fn scan_workspace_extensions(workspace: &Path) -> Vec<WorkspaceExtension> {
    let mut found = Vec::new();
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else { continue; };
        let kind = detect_extension_kind(&directory).ok();
        if let Some(kind) = kind {
            let metadata = workspace_extension_metadata(&kind, &directory);
            let relative_path = directory.strip_prefix(workspace).ok().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
            match metadata {
                Ok((name, version, description)) => found.push(WorkspaceExtension { kind, name, version, description, relative_path, content_digest: extension_digest(&directory).unwrap_or_else(|error| format!("unavailable:{error}")), diagnostic: None }),
                Err(error) => found.push(WorkspaceExtension { kind, name: directory.file_name().and_then(|item| item.to_str()).unwrap_or("extension").to_owned(), version: None, description: None, relative_path, content_digest: String::new(), diagnostic: Some(error) }),
            }
            continue;
        }
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | "node_modules" | "dist" | "build" | ".cache" | ".dsh")) { continue; }
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) { pending.push(entry.path()); }
        }
    }
    found.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    found
}

fn workspace_extension_metadata(kind: &ExtensionKind, source: &Path) -> Result<(String, Option<String>, Option<String>), String> {
    match kind {
        ExtensionKind::Skill => {
            let content = fs::read_to_string(source.join("SKILL.md")).map_err(|error| error.to_string())?;
            let field = |key: &str| content.lines().find_map(|line| line.strip_prefix(key).map(str::trim)).map(|value| value.trim_matches(['\'', '"']).to_owned());
            Ok((field("name:").ok_or("skill frontmatter has no name")?, None, field("description:")))
        }
        ExtensionKind::Plugin => {
            let value: Value = serde_json::from_str(&fs::read_to_string(source.join("package.json")).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
            Ok((value["name"].as_str().ok_or("plugin package.json has no name")?.to_owned(), value["version"].as_str().map(str::to_owned), value["description"].as_str().map(str::to_owned)))
        }
    }
}

pub fn repository_root(runtime: &Path) -> PathBuf {
    runtime.join("repository")
}

pub fn repository_index_path(runtime: &Path) -> PathBuf {
    repository_root(runtime).join("index.json")
}

/// Reads the index and verifies every source still exists. Invalid entries remain visible as diagnostics.
pub fn scan_repository(runtime: &Path) -> Vec<RepositoryExtension> {
    let path = repository_index_path(runtime);
    let mut entries: Vec<RepositoryExtension> = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    for entry in &mut entries {
        let source = Path::new(&entry.source_path);
        if !source.is_dir() {
            entry.diagnostic = Some("repository source directory is missing".to_owned());
        } else if let Err(error) = detect_extension_kind(source) {
            entry.diagnostic = Some(error);
        } else {
            entry.content_digest = extension_digest(source).unwrap_or_else(|error| format!("unavailable:{error}"));
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    entries
}

pub fn write_repository_index(runtime: &Path, entries: &[RepositoryExtension]) -> Result<(), String> {
    let path = repository_index_path(runtime);
    fs::create_dir_all(path.parent().ok_or("repository index has no parent")?).map_err(|error| error.to_string())?;
    fs::write(path, serde_json::to_string_pretty(entries).map_err(|error| error.to_string())?).map_err(|error| error.to_string())
}

// ── Owner-id references ───────────────────────────────────────────────────
// Persistent per-entry owner-id map stored at
// `<root>/repository/references.json`. Each entry tracks which containers
// and which built templates currently link it; an entry is removable only
// when both sets are empty, so a plugin referenced by a template (but
// never installed into a container) survives `plugin prune`.
//
// The on-disk shape is `{containers: [id1, id2], templates: [id3]}`. The
// in-memory type is `BTreeSet<String>`, so `add(insert) / remove(erase)`
// are no-ops when the id is already (or no longer) present — we never
// have to reason about saturation or drift.
//
// Reads are unverified: callers get whatever is on disk. Every write
// (add / remove / prune / rm / template delete / container delete) goes
// through `reconcile_owner_index` first, which rebuilds the map from the
// canonical sources (each container's `extensions.json` and each built
// template's `list.json`) and prunes / adds anything stale. That makes
// the index crash-safe: a missing or torn file is fixed the next time
// the user mutates anything, with no separate "gc" command required.

/// Set of owner ids (containers or templates) referencing one entry.
pub type OwnerSet = BTreeSet<String>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCount {
    #[serde(default)]
    pub templates: OwnerSet,
}

impl ReferenceCount {
    /// Total number of owners (containers + templates). Kept as `u32`
    /// for back-compat with the old numeric snapshot.
    pub fn total(&self) -> u32 {
        self.templates.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Returns true when the id was newly inserted.
    pub fn add(&mut self, kind: ReferenceKind, owner_id: &str) -> bool {
        let _ = kind;
        self.templates.insert(owner_id.to_owned())
    }

    /// Returns true when the id was actually removed.
    pub fn remove(&mut self, kind: ReferenceKind, owner_id: &str) -> bool {
        let _ = kind;
        self.templates.remove(owner_id)
    }
}

/// Which owner is gaining or losing a reference. Container references are
/// recorded against one container's `extensions.json`; template references
/// are recorded against one built template's `list.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    Template,
}

/// Summary returned by `reconcile_owner_index` so callers / tests can
/// confirm drift was repaired.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub owners_rebuilt: usize,
    pub containers_added: usize,
    pub containers_pruned: usize,
    pub templates_added: usize,
    pub templates_pruned: usize,
}

pub fn references_path(runtime: &Path) -> PathBuf {
    repository_root(runtime).join("references.json")
}

/// Read the persisted owner-id map. The legacy numeric format
/// (`{"id": 3}`) is accepted but ignored — its value is dropped and the
/// entry reads as empty. The next write upgrades the on-disk shape.
/// Missing or malformed files read as empty, mirroring `scan_repository`.
pub fn read_references(runtime: &Path) -> BTreeMap<String, ReferenceCount> {
    let raw = match fs::read_to_string(references_path(runtime)) {
        Ok(text) => text,
        Err(_) => return BTreeMap::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    let Some(object) = value.as_object() else {
        return out;
    };
    for (key, entry) in object {
        // Legacy: bare numeric value (old single-counter format). Drop it.
        if entry.is_number() {
            continue;
        }
        // Legacy v2: `{"containers": N, "templates": M}` — numeric ids were
        // never valid, so treat the numbers as zero and rewrite on next save.
        if let Some(object) = entry.as_object() {
            let mut rebuilt = ReferenceCount::default();
            if let Some(arr) = object.get("templates").and_then(Value::as_array) {
                for item in arr {
                    if let Some(id) = item.as_str() {
                        rebuilt.templates.insert(id.to_owned());
                    }
                }
            }
            out.insert(key.clone(), rebuilt);
        }
    }
    out
}

pub fn write_references(
    runtime: &Path,
    references: &BTreeMap<String, ReferenceCount>,
) -> Result<(), String> {
    let path = references_path(runtime);
    fs::create_dir_all(path.parent().ok_or("references has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string_pretty(references).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// Record that `owner_id` now references `entry_id`. Idempotent: inserting
/// an id that already exists is a no-op (returns `false`), so callers do
/// not need to read-modify-write defensively.
pub fn add_reference_owner(
    runtime: &Path,
    entry_id: &str,
    kind: ReferenceKind,
    owner_id: &str,
) -> Result<(), String> {
    let mut references = read_references(runtime);
    let entry = references.entry(entry_id.to_owned()).or_default();
    entry.add(kind, owner_id);
    write_references(runtime, &references)
}

/// Record that `owner_id` no longer references `entry_id`. Idempotent:
/// removing an absent id is a no-op, so the call never panics on drift.
pub fn remove_reference_owner(
    runtime: &Path,
    entry_id: &str,
    kind: ReferenceKind,
    owner_id: &str,
) -> Result<(), String> {
    let mut references = read_references(runtime);
    if let Some(entry) = references.get_mut(entry_id) {
        entry.remove(kind, owner_id);
        if entry.is_empty() {
            references.remove(entry_id);
        }
    }
    write_references(runtime, &references)
}

/// Rebuild the on-disk owner-id map from the canonical sources (every
/// container's `extensions.json` and every built template's `list.json`)
/// and write it back. Call this before any mutation so a torn file, a
/// crash between the half-step of "install plugin" and "record owner",
/// or a manual edit never produces a permanently-stuck counter.
///
/// Returns a report of what changed so callers can log it. The function
/// is best-effort about per-source failures (a corrupt `extensions.json`
/// or `list.json` is logged and skipped) — the rest of the system still
/// gets rebuilt.
pub fn reconcile_owner_index(runtime: &Path) -> Result<ReconcileReport, String> {
    let root_str = runtime
        .to_str()
        .ok_or_else(|| "runtime directory is not valid UTF-8".to_owned())?;

    let mut truth: BTreeMap<String, ReferenceCount> = BTreeMap::new();

    // Template-side owners: every built template's `list.json` Reference
    // resource counts as one template owner for the referenced plugin.
    let template_index = box_dsh_versions::read_template_index(root_str);
    for (name, entry) in &template_index {
        if !entry.built {
            continue;
        }
        let list = match box_dsh_versions::read_built_template(root_str, name) {
            Ok(Some(list)) => list,
            Ok(None) => continue,
            Err(_) => continue,
        };
        for resource in &list.resources {
            if resource.source_kind != "plugin" {
                continue;
            }
            // v8 manifests contain artifact-local paths and content hashes,
            // not a repository id. Resolve the build-cache bookkeeping row
            // by immutable package identity instead of reintroducing a path
            // reference into the template format.
            if let Some(repository_entry) = scan_repository(runtime).into_iter().find(|candidate| {
                candidate.kind == ExtensionKind::Plugin
                    && candidate.name == resource.name
                    && candidate.content_digest == resource.sha256
            }) {
                truth
                    .entry(repository_entry.id)
                    .or_default()
                    .templates
                    .insert(entry.id.clone());
            }
        }
    }

    // Diff against the current on-disk map so we can report drift.
    let current = read_references(runtime);
    let mut report = ReconcileReport::default();
    report.owners_rebuilt = truth.len();
    for (id, true_set) in &truth {
        let cur = current.get(id);
        let templates_added = match cur {
            Some(prev) => true_set.templates.difference(&prev.templates).count(),
            None => true_set.templates.len(),
        };
        let templates_pruned = match cur {
            Some(prev) => prev.templates.difference(&true_set.templates).count(),
            None => 0,
        };
        report.templates_added += templates_added;
        report.templates_pruned += templates_pruned;
    }
    // Owners present on disk but with no current reference are pruned too;
    // we count them by walking the current map.
    for (id, cur) in &current {
        if truth.contains_key(id) {
            continue;
        }
        report.templates_pruned += cur.templates.len();
    }

    write_references(runtime, &truth)?;
    Ok(report)
}

/// Repository ids whose reference set is empty (no container AND no
/// template) — candidates for `remove_repository_extension`. Entries
/// absent from the map count as unused, so a fresh store prunes nothing
/// extra.
pub fn unused_repository_ids(runtime: &Path) -> Vec<String> {
    let references = read_references(runtime);
    let entries = scan_repository(runtime);
    entries
        .into_iter()
        .filter(|entry| references.get(&entry.id).map(|count| count.is_empty()).unwrap_or(true))
        .map(|entry| entry.id)
        .collect()
}

/// How many owners currently reference `entry_id` (0 when absent).
/// Sums both container and template references.
pub fn reference_count(runtime: &Path, entry_id: &str) -> u32 {
    read_references(runtime)
        .get(entry_id)
        .map(|count| count.total())
        .unwrap_or(0)
}

/// One row of the owner-detail payload surfaced by
/// `list_repository_reference_counts`. The on-disk `ReferenceCount` keeps
/// the raw `BTreeSet<String>`; this struct is the wire-shape view that
/// the desktop and CLI consume so they can render "used by container X
/// and template Y" without having to round-trip through the daemon for
/// every entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryReferenceRow {
    pub id: String,
    pub kind: ExtensionKind,
    pub name: String,
    pub version: Option<String>,
    #[serde(default)]
    pub containers: Vec<String>,
    #[serde(default)]
    pub templates: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BundleEntry {
    /// Repository id the entry was picked from; quick exports keep GitHub
    /// entries as URLs while full exports embed their content.
    pub repository_id: String,
    pub kind: ExtensionKind,
    pub name: String,
    pub version: Option<String>,
    /// Original import source of the entry (GitHub URL or local path).
    #[serde(default)]
    pub source: Option<String>,
    /// Content size in bytes of the entry directory (0 when unavailable).
    pub size: u64,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

/// A named, persisted collection of repository extensions mixed across
/// plugins and skills, exported as a single tarball.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionBundle {
    pub id: String,
    pub name: String,
    pub entries: Vec<BundleEntry>,
    pub created_at: u64,
}

pub fn bundles_path(runtime: &Path) -> PathBuf {
    runtime.join("state").join("bundles.json")
}

pub fn read_bundles(runtime: &Path) -> Vec<ExtensionBundle> {
    fs::read_to_string(bundles_path(runtime))
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
        .unwrap_or_default()
}

pub fn write_bundles(runtime: &Path, bundles: &[ExtensionBundle]) -> Result<(), String> {
    let path = bundles_path(runtime);
    fs::create_dir_all(path.parent().ok_or("bundle state has no parent")?).map_err(|error| error.to_string())?;
    fs::write(path, serde_json::to_string_pretty(bundles).map_err(|error| error.to_string())?).map_err(|error| error.to_string())
}

/// Total size of a directory tree in bytes, skipping the same noisy folders
/// that are never packaged (VCS, dependencies, build output).
pub fn directory_size(root: &Path) -> u64 {
    fn visit(current: &Path, total: &mut u64) {
        let Ok(entries) = fs::read_dir(current) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some(".git" | "node_modules" | "dist" | "build" | ".cache" | ".dsh")
            ) {
                continue;
            }
            let path = entry.path();
            if let Ok(kind) = entry.file_type() {
                if kind.is_dir() {
                    visit(&path, total);
                } else if kind.is_file() {
                    if let Ok(metadata) = entry.metadata() {
                        *total += metadata.len();
                    }
                }
            }
        }
    }
    let mut total = 0;
    visit(root, &mut total);
    total
}

/// Stable content digest excluding dependency and VCS directories.
pub fn extension_digest(root: &Path) -> Result<String, String> {
    fn visit(root: &Path, current: &Path, bytes: &mut Vec<u8>) -> Result<(), String> {
        let mut entries = fs::read_dir(current).map_err(|error| error.to_string())?.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | "node_modules")) { continue; }
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
            bytes.extend_from_slice(relative.to_string_lossy().as_bytes());
            if entry.file_type().map_err(|error| error.to_string())?.is_dir() { visit(root, &path, bytes)?; }
            else if path.is_file() { bytes.extend_from_slice(&fs::read(&path).map_err(|error| error.to_string())?); }
        }
        Ok(())
    }
    let mut bytes = Vec::new(); visit(root, root, &mut bytes)?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes { hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3); }
    Ok(format!("fnv1a64:{hash:016x}"))
}

pub fn detect_extension_kind(directory: &Path) -> Result<ExtensionKind, String> {
    if directory.join("SKILL.md").is_file() {
        return Ok(ExtensionKind::Skill);
    }
    let manifest = directory.join("package.json");
    let value: Value = serde_json::from_str(
        &fs::read_to_string(&manifest)
            .map_err(|_| "extension has neither SKILL.md nor package.json".to_owned())?,
    )
    .map_err(|error| format!("cannot parse plugin package.json: {error}"))?;
    if value
        .pointer("/dsh/bundle/patch")
        .and_then(Value::as_str)
        .is_some()
    {
        Ok(ExtensionKind::Plugin)
    } else {
        Err("package.json does not declare dsh.bundle.patch".to_owned())
    }
}

pub fn extension_records_path(container: &DshContainer) -> PathBuf {
    PathBuf::from(&container.directory).join("state/extensions.json")
}

pub fn read_extension_records(container: &DshContainer) -> Vec<ExtensionRecord> {
    fs::read_to_string(extension_records_path(container))
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
        .unwrap_or_default()
}

pub fn write_extension_record(
    container: &DshContainer,
    record: ExtensionRecord,
) -> Result<(), String> {
    let path = extension_records_path(container);
    fs::create_dir_all(path.parent().ok_or("extension registry has no parent")?)
        .map_err(|error| error.to_string())?;
    let mut records = read_extension_records(container);
    records.retain(|item| {
        !(item.kind == record.kind && item.name == record.name && item.profile == record.profile)
    });
    records.push(record);
    fs::write(
        path,
        serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// Removes the persisted record for one plugin installed into a profile.
pub fn remove_plugin_record(
    container: &DshContainer,
    profile: &str,
    name: &str,
) -> Result<(), String> {
    let path = extension_records_path(container);
    let mut records = read_extension_records(container);
    records.retain(|item| {
        !(item.kind == ExtensionKind::Plugin
            && item.profile.as_deref() == Some(profile)
            && item.name == name)
    });
    fs::write(
        path,
        serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlugin {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub path: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileExtensions {
    pub name: String,
    pub plugins: Vec<ExtensionPlugin>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSkill {
    pub name: String,
    pub description: Option<String>,
    pub path: String,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerExtensions {
    pub container_id: String,
    pub profiles: Vec<ProfileExtensions>,
    pub skills: Vec<ContainerSkill>,
    pub diagnostics: Vec<String>,
    pub scanned_at: u64,
}

/// Scans only data owned by one container; project and runtime skill roots are excluded.
pub fn scan_container_extensions(container: &DshContainer) -> ContainerExtensions {
    let profile_root = PathBuf::from(&container.directory).join("profile");
    let mut details = ContainerExtensions {
        container_id: container.id.clone(),
        profiles: Vec::new(),
        skills: Vec::new(),
        diagnostics: Vec::new(),
        scanned_at: now_seconds(),
    };
    details.profiles = scan_profiles(&profile_root, &mut details.diagnostics);
    details.skills = scan_skills(&profile_root.join("skills"), &mut details.diagnostics);
    details
}

fn scan_profiles(root: &Path, diagnostics: &mut Vec<String>) -> Vec<ProfileExtensions> {
    let directory = root.join("profiles");
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut profiles = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "node_modules" {
                return None;
            }
            Some(scan_profile(
                &name,
                &entry.path(),
                &directory.join("node_modules"),
            ))
        })
        .collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    if profiles.is_empty() && directory.exists() {
        diagnostics.push("no DSH profiles found".to_owned());
    }
    profiles
}

fn scan_profile(name: &str, directory: &Path, shared_modules: &Path) -> ProfileExtensions {
    let manifest = directory.join("package.json");
    let mut result = ProfileExtensions {
        name: name.to_owned(),
        plugins: Vec::new(),
        diagnostics: Vec::new(),
    };
    let content = match fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(error) => {
            result
                .diagnostics
                .push(format!("cannot read {}: {error}", manifest.display()));
            return result;
        }
    };
    let value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            result
                .diagnostics
                .push(format!("cannot parse {}: {error}", manifest.display()));
            return result;
        }
    };
    let bundles = value
        .pointer("/dsh/profile/bundles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for bundle in bundles {
        let Some(package) = bundle.as_str() else {
            result
                .diagnostics
                .push("profile bundle is not a package name".to_owned());
            continue;
        };
        result
            .plugins
            .push(read_plugin(package, directory, shared_modules));
    }
    result
}

fn read_plugin(package: &str, profile: &Path, shared_modules: &Path) -> ExtensionPlugin {
    let local = profile
        .join("node_modules")
        .join(package)
        .join("package.json");
    let shared = shared_modules.join(package).join("package.json");
    let manifest = [local, shared].into_iter().find(|path| path.is_file());
    let Some(manifest) = manifest else {
        return ExtensionPlugin {
            name: package.to_owned(),
            version: None,
            description: None,
            path: None,
            diagnostic: Some("package is declared by the profile but is not installed".to_owned()),
        };
    };
    let content = match fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(error) => {
            return ExtensionPlugin {
                name: package.to_owned(),
                version: None,
                description: None,
                path: Some(manifest.to_string_lossy().into_owned()),
                diagnostic: Some(format!("cannot read package metadata: {error}")),
            }
        }
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(value) => ExtensionPlugin {
            name: value["name"].as_str().unwrap_or(package).to_owned(),
            version: value["version"].as_str().map(str::to_owned),
            description: value["description"].as_str().map(str::to_owned),
            path: Some(
                manifest
                    .parent()
                    .unwrap_or(&manifest)
                    .to_string_lossy()
                    .into_owned(),
            ),
            diagnostic: None,
        },
        Err(error) => ExtensionPlugin {
            name: package.to_owned(),
            version: None,
            description: None,
            path: Some(manifest.to_string_lossy().into_owned()),
            diagnostic: Some(format!("cannot parse package metadata: {error}")),
        },
    }
}

fn scan_skills(root: &Path, diagnostics: &mut Vec<String>) -> Vec<ContainerSkill> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let candidate = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
            path
        } else {
            continue;
        };
        if candidate.file_name().and_then(|value| value.to_str()) != Some("SKILL.md")
            && candidate.parent() != Some(root)
        {
            continue;
        }
        skills.push(read_skill(&candidate));
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    if !root.exists() {
        return skills;
    }
    if skills.is_empty() && root.is_dir() {
        diagnostics.push("no container skills found".to_owned());
    }
    skills
}

fn read_skill(path: &Path) -> ContainerSkill {
    let fallback = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unnamed-skill")
        .to_owned();
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            return ContainerSkill {
                name: fallback,
                description: None,
                path: path.to_string_lossy().into_owned(),
                diagnostic: Some(format!("cannot read skill: {error}")),
            }
        }
    };
    let (name, description, diagnostic) =
        parse_frontmatter(&content).unwrap_or_else(|error| (fallback, None, Some(error)));
    ContainerSkill {
        name,
        description,
        path: path.to_string_lossy().into_owned(),
        diagnostic,
    }
}

fn parse_frontmatter(content: &str) -> Result<(String, Option<String>, Option<String>), String> {
    let Some(body) = content.strip_prefix("---\n") else {
        return Err("missing YAML frontmatter".to_owned());
    };
    let Some((frontmatter, _)) = body.split_once("\n---") else {
        return Err("unterminated YAML frontmatter".to_owned());
    };
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']).to_owned();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    Ok((
        name.ok_or("skill frontmatter has no name")?,
        description,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    fn container(root: &Path) -> DshContainer {
        DshContainer {
            id: "one".to_owned(),
            name: "One".to_owned(),
            version: "latest".to_owned(),
            profile: "web".to_owned(),
            template: None,
            directory: root.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        }
    }

    #[test]
    fn scans_profiles_scoped_plugins_and_container_skills() {
        let root = std::env::temp_dir().join(format!("dshbox-extension-test-{}", now_seconds()));
        fs::create_dir_all(root.join("profile/profiles/web/node_modules/@scope/plugin")).unwrap();
        fs::create_dir_all(root.join("profile/skills/demo")).unwrap();
        fs::write(
            root.join("profile/profiles/web/package.json"),
            r#"{"dsh":{"profile":{"bundles":["@scope/plugin","missing"]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/profiles/web/node_modules/@scope/plugin/package.json"),
            r#"{"name":"@scope/plugin","version":"1.2.3","description":"Plugin"}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\n",
        )
        .unwrap();
        let found = scan_container_extensions(&container(&root));
        assert_eq!(
            found.profiles[0].plugins[0].version.as_deref(),
            Some("1.2.3")
        );
        assert!(found.profiles[0].plugins[1].diagnostic.is_some());
        assert_eq!(found.skills[0].name, "demo");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reference_owners_are_added_removed_and_idempotent() {
        let root = std::env::temp_dir().join(format!("dshbox-references-test-{}", now_seconds()));
        fs::create_dir_all(repository_root(&root)).unwrap();

        // Absent entries count as zero.
        assert_eq!(reference_count(&root, "img-a"), 0);
        assert!(unused_repository_ids(&root).is_empty());

        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert_eq!(reference_count(&root, "img-a"), 1);

        // Persistence: a fresh read sees both sets.
        let snapshot = read_references(&root);
        assert_eq!(snapshot["img-a"].templates.len(), 1);

        // Empty entries are dropped from the on-disk map so the file
        // stays compact for the unused check.
        remove_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert!(read_references(&root).get("img-a").is_none());

        // An entry with at least one owner survives unused_repository_ids.
        write_repository_index(
            &root,
            &[RepositoryExtension {
                id: "img-a".to_owned(),
                kind: ExtensionKind::Plugin,
                name: "a".to_owned(),
                version: None,
                description: None,
                content_digest: "d".to_owned(),
                source_path: "missing".to_owned(),
                imported_at: 0,
                diagnostic: None,
                source: None,
            }],
        )
        .unwrap();
        assert_eq!(unused_repository_ids(&root), vec!["img-a"]);
        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert!(unused_repository_ids(&root).is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_numeric_references_are_dropped_on_read() {
        let root = std::env::temp_dir().join(format!("dshbox-references-legacy-{}", now_seconds()));
        fs::create_dir_all(repository_root(&root)).unwrap();
        // Oldest format: bare numeric value. Reads as empty; a subsequent
        // write drops the entry from the file entirely.
        std::fs::write(references_path(&root), "{\"img-a\": 3}").unwrap();
        assert!(read_references(&root).is_empty());

        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-x").unwrap();
        let raw = std::fs::read_to_string(references_path(&root)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["img-a"]["templates"], serde_json::json!(["tpl-x"]));
        assert!(parsed["img-a"].get("containers").is_none()
            || parsed["img-a"]["templates"].as_array().unwrap().len() == 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_rebuilds_owner_map_from_canonical_sources() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-reconcile-test-{}",
            now_seconds()
        ));
        let home = std::env::temp_dir().join(format!("dshbox-reconcile-home-{}", now_seconds()));
        fs::create_dir_all(repository_root(&root)).unwrap();
        fs::create_dir_all(home.join(".dsh-box")).unwrap();
        fs::write(
            home.join(".dsh-box/config.json"),
            serde_json::json!({ "runtimeDirectory": root.to_string_lossy() }).to_string(),
        ).unwrap();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: single-threaded test, restored on drop.
        unsafe { std::env::set_var("HOME", &home) };

        // Build a container workspace with one repository-linked plugin.
        let container_root = root.join("instances/container-alpha");
        fs::create_dir_all(&container_root).unwrap();
        // `box_containers::scan_containers` only returns containers
        // that have a `container.json` sidecar; write one so the
        // reconcile pass sees `container-alpha` as a live owner.
        fs::write(
            container_root.join("container.json"),
            serde_json::json!({
                "id": "container-alpha",
                "name": "alpha",
                "version": "v1",
                "profile": "web",
                "directory": container_root.to_string_lossy(),
                "status": "stopped",
            })
            .to_string(),
        )
        .unwrap();
        let record = ExtensionRecord {
            kind: ExtensionKind::Plugin,
            name: "@scope/repo-plugin".to_owned(),
            source_kind: "repository".to_owned(),
                source: "img-real".to_owned(),
                profile: Some("web".to_owned()),
                path: "/repo/path".to_owned(),
                installed_at: now_seconds(),
                repository_id: Some("img-real".to_owned()),
                content_digest: None,
            };
            write_extension_record(
                &DshContainer {
                    id: "container-alpha".to_owned(),
                    name: "alpha".to_owned(),
                    version: "v1".to_owned(),
                    profile: "web".to_owned(),
                    template: None,
                    directory: container_root.to_string_lossy().into_owned(),
                    status: "stopped".to_owned(),
                },
                record,
            )
            .unwrap();

        // Pre-seed references.json with stale entries that disagree with
        // the canonical sources.
        let mut seeded = BTreeMap::new();
        seeded.insert(
            "img-orphan".to_owned(),
            ReferenceCount {
                templates: BTreeSet::from(["tpl-orphan".to_owned()]),
                ..ReferenceCount::default()
            },
        );
        write_references(&root, &seeded).unwrap();

        let report = reconcile_owner_index(&root).unwrap();
        // `img-orphan` has no canonical reference, so the whole entry
        // (its template owner set) is wiped from the on-disk map.
        assert_eq!(report.templates_pruned, 1, "orphan entry dropped");

        let after = read_references(&root);
        assert!(after.get("img-orphan").is_none());

        if let Some(value) = prev_home {
            unsafe { std::env::set_var("HOME", value) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detects_skill_and_dsh_plugin_roots() {
        let root = std::env::temp_dir().join(format!("dshbox-detect-test-{}", now_seconds()));
        let skill = root.join("skill");
        let plugin = root.join("plugin");
        fs::create_dir_all(&skill).unwrap();
        fs::create_dir_all(&plugin).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: skill\n---\n").unwrap();
        fs::write(
            plugin.join("package.json"),
            r#"{"dsh":{"bundle":{"patch":"./cordis.patch.yml"}}}"#,
        )
        .unwrap();
        assert_eq!(detect_extension_kind(&skill).unwrap(), ExtensionKind::Skill);
        assert_eq!(
            detect_extension_kind(&plugin).unwrap(),
            ExtensionKind::Plugin
        );
        let _ = fs::remove_dir_all(root);
    }
}
