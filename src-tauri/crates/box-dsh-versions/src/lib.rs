//! DSH runtime DTOs, source repository constants, and the auto-generation
//! of base templates for every installed DSH version.

use box_foundation::{is_safe_identifier, mirror_url, now_seconds, read_config, BoxResult};
use box_runtime::{remove_checkout, shallow_clone_with_cancel};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
};

pub const DSH_REPOSITORY: &str = "https://github.com/deepseek-ai/deepseek-harness.git";
pub const DSH_TAGS_API: &str =
    "https://api.github.com/repos/deepseek-ai/deepseek-harness/tags?per_page=100";

/// Canonical FROM reference used by every base template that this crate
/// auto-generates. The `harness` Resources tab is now just a UI alias for
/// these templates — there is no separate harness resource type any more.
pub const HARNESS_STANDARD_REF: &str = "github.com/deepseek-ai/deepseek-harness:latest";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshVersion {
    pub name: String,
    pub installed: bool,
}

/// What one migration pass produced for a single installed DSH version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUpgradeReport {
    pub version: String,
    /// Absolute path of the base template this version owns.
    pub template_path: String,
    /// The base template was created by this pass. Existing templates are
    /// never overwritten, so a second run reports `false` for every version.
    pub template_created: bool,
}

/// A `pull template <ref>` request broken into its constituent pieces. The
/// `version` is what gets used as the install directory name and the local
/// template file name (`<root>/templates/<version>.dsh`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    /// Full `https://...` URL of the Git repository.
    pub url: String,
    /// Optional tag/branch/commit to check out; `None` means `latest`.
    pub tag: Option<String>,
    /// The version slug persisted as the install directory name. Defaults
    /// to `latest` when the caller omits a tag.
    pub version: String,
}

/// Parse a `github.com/<owner>/<repo>[:tag|@ref]` style reference into the
/// pieces needed to clone the repository and name the local install. The
/// `:tag` / `@ref` suffix is optional; when absent, `latest` is used. The
/// returned `url` is always `https://github.com/<owner>/<repo>` (the host
/// has to be one of the well-known git hosts, since `shallow_clone_with_cancel`
/// only handles `https://` remotes).
pub fn parse_template_ref(ref_value: &str) -> BoxResult<TemplateRef> {
    let value = ref_value.trim();
    if value.is_empty() {
        return Err("template reference cannot be empty".to_owned());
    }
    let last_at = value.rfind('@');
    let last_colon = value.rfind(':');
    let (head, tag) = match (last_at, last_colon) {
        (Some(at), Some(colon)) if at > colon => {
            let tag = &value[at + 1..];
            if tag.is_empty() {
                return Err("template reference has empty `@ref` suffix".to_owned());
            }
            (&value[..at], Some(tag.to_owned()))
        }
        (Some(at), None) => {
            let tag = &value[at + 1..];
            if tag.is_empty() {
                return Err("template reference has empty `@ref` suffix".to_owned());
            }
            (&value[..at], Some(tag.to_owned()))
        }
        (Some(_), Some(colon)) => {
            let tag = &value[colon + 1..];
            if tag.is_empty() {
                return Err("template reference has empty `:tag` suffix".to_owned());
            }
            (&value[..colon], Some(tag.to_owned()))
        }
        (None, Some(colon)) => {
            let tag = &value[colon + 1..];
            if tag.is_empty() {
                return Err("template reference has empty `:tag` suffix".to_owned());
            }
            (&value[..colon], Some(tag.to_owned()))
        }
        (None, None) => (value, None),
    };
    let parts: Vec<&str> = head.split('/').collect();
    if parts.len() != 3 || !matches!(parts[0], "github.com" | "gitlab.com" | "bitbucket.org") {
        return Err(format!(
            "template reference must be `github.com/<owner>/<repo>[:tag|@ref]`, got `{ref_value}`"
        ));
    }
    let url = format!("https://{head}");
    let version = tag.clone().unwrap_or_else(|| "latest".to_owned());
    if !is_safe_identifier(&version) {
        return Err(format!(
            "template reference resolves to an unsafe version `{version}`"
        ));
    }
    Ok(TemplateRef { url, tag, version })
}

pub fn version_directory(root: &str, version: &str) -> PathBuf {
    PathBuf::from(root)
        .join("runtimes")
        .join(version)
        .join("source")
}

/// `<root>/templates` — base `.dsh` scripts, one per installed DSH version.
pub fn templates_directory(root: &str) -> PathBuf {
    PathBuf::from(root).join("templates")
}

/// `<root>/templates/<version>.dsh` — base build script for a version.
pub fn harness_template_path(root: &str, version: &str) -> PathBuf {
    templates_directory(root).join(format!("{version}.dsh"))
}

/// Collapse a template reference (`github.com/<owner>/<repo>[:tag|@ref]`)
/// into a filesystem-safe filename stem. The actual template body is
/// stored in a content-addressable hash directory; this legacy alias is
/// only kept around so `build_image` can still resolve templates by their
/// old version (`latest`, `v0.1.0`) without re-plumbing the build path.
pub fn sanitize_template_ref(ref_value: &str) -> String {
    // Older builds split the URL on `/` and `:` to make a flat filename
    // like `github.com_deepseek-ai_deepseek-harness_latest.dsh`. The new
    // layout stores the body under a hash directory, so this is only used
    // as a friendly alias stem; the user-facing name in the index keeps
    // the original `/` and `:` characters verbatim.
    ref_value.trim().replace('\\', "_")
}

/// `<root>/templates/<ref-sanitized>.dsh` — pull_template writes its base
/// script here so two different refs (e.g. `.../harness:latest` and
/// `.../harness:v0.1.0`) live side by side instead of overwriting each other.
pub fn template_path(root: &str, ref_value: &str) -> PathBuf {
    templates_directory(root).join(format!("{}.dsh", sanitize_template_ref(ref_value)))
}

/// Per-runtime template storage: every distinct script lives at
/// `<root>/templates/<fnv1a64-hash>/script.dsh` and the index at
/// `<root>/state/template-index.json` maps the user-facing `name` (an alias
/// like `deepseek-harness:latest`) to the content-addressable id. Pull and
/// import flows land here, mirroring the plugin resource's two-level
/// (index + content-addressable directory) layout.
pub fn template_storage_root(root: &str) -> PathBuf {
    templates_directory(root)
}

pub fn template_index_path(root: &str) -> PathBuf {
    PathBuf::from(root).join("state").join("template-index.json")
}

pub fn template_content_path(root: &str, id: &str) -> PathBuf {
    template_storage_root(root).join(id).join("script.dsh")
}

pub fn template_manifest_path(root: &str, id: &str) -> PathBuf {
    template_storage_root(root).join(id).join("manifest.json")
}

/// FNV-1a 64-bit hash of a single template script. The content is the
/// script body verbatim (no `fnv1a64:` prefix here; the prefix is added by
/// `extension_digest` and we deliberately keep template ids bare so they
/// read naturally in `template ls` and on disk).
pub fn template_content_hash(content: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in content.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Index entry for one local template. `id` is the content-addressable
/// hash; `name` is the user-facing alias used by `template rm/show/export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateEntry {
    pub name: String,
    pub id: String,
    #[serde(default)]
    pub harness_ref: Option<String>,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub imported_at: u64,
    #[serde(default)]
    pub from_ref: Option<String>,
    /// True when the hash directory holds a built resource list
    /// (`list.json`) instead of a source script (`script.dsh`).
    #[serde(default)]
    pub built: bool,
}

pub type TemplateIndex = std::collections::BTreeMap<String, TemplateEntry>;

pub fn read_template_index(root: &str) -> TemplateIndex {
    fs::read_to_string(template_index_path(root))
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
        .unwrap_or_default()
}

pub fn write_template_index(root: &str, index: &TemplateIndex) -> Result<(), String> {
    let path = template_index_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(index).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// Persist a script body to the hash-addressed template store, write its
/// manifest, and register it under `name` in the index. Returns the new
/// entry. If the same name was previously registered under a different
/// hash, that orphan directory is garbage-collected.
pub fn write_template_with_entry(
    root: &str,
    name: &str,
    text: &str,
    harness_ref: Option<String>,
    profile: &str,
    from_ref: Option<String>,
    imported_at: u64,
) -> Result<TemplateEntry, String> {
    let id = template_content_hash(text);
    let dir = template_storage_root(root).join(&id);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    fs::write(dir.join("script.dsh"), text).map_err(|error| error.to_string())?;
    let entry = TemplateEntry {
        name: name.to_owned(),
        id: id.clone(),
        harness_ref,
        profile: profile.to_owned(),
        imported_at,
        from_ref,
        built: false,
    };
    let manifest = serde_json::json!({
        "name": entry.name,
        "id": entry.id,
        "harnessRef": entry.harness_ref,
        "profile": entry.profile,
        "importedAt": entry.imported_at,
        "fromRef": entry.from_ref,
    });
    fs::write(
        template_manifest_path(root, &id),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut index = read_template_index(root);
    if let Some(previous) = index.insert(name.to_owned(), entry.clone()) {
        if previous.id != entry.id {
            collect_unreferenced_template_hash(root, &previous.id, &index);
        }
    }
    write_template_index(root, &index)?;
    Ok(entry)
}

/// Delete the hash-addressed directory and its manifest when no index entry
/// still points at it. Used both by `write_template_with_entry` (to retire
/// the old hash when the same name is re-imported with different bytes) and
/// by `remove_template` (when the user drops a template).
pub fn collect_unreferenced_template_hash(root: &str, id: &str, index: &TemplateIndex) {
    if index.values().any(|entry| entry.id == id) {
        return;
    }
    let dir = template_storage_root(root).join(id);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_file(template_manifest_path(root, id));
}

pub fn built_template_list_path(root: &str, id: &str) -> PathBuf {
    template_storage_root(root).join(id).join("list.json")
}

/// Persist a built template (the metadata-only product of `dshbox build`)
/// into the SAME content-addressable store as source script templates —
/// there is no separate image registry, only built templates. The id is
/// the fnv1a64 hash of the serialised resource list; re-building under
/// the same name retires the previous hash.
pub fn write_built_template(
    root: &str,
    list: &box_api::TemplateResourceList,
) -> Result<TemplateEntry, String> {
    let body = serde_json::to_string(list).map_err(|error| error.to_string())?;
    let id = template_content_hash(&body);
    let dir = template_storage_root(root).join(&id);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    fs::write(dir.join("list.json"), &body).map_err(|error| error.to_string())?;
    let entry = TemplateEntry {
        name: list.name.clone(),
        id: id.clone(),
        harness_ref: list.harness_ref.clone(),
        profile: list.profile.clone(),
        imported_at: list.created_at,
        from_ref: Some(list.base.clone()),
        built: true,
    };
    let manifest = serde_json::json!({
        "name": entry.name,
        "id": entry.id,
        "harnessRef": entry.harness_ref,
        "profile": entry.profile,
        "importedAt": entry.imported_at,
        "fromRef": entry.from_ref,
        "built": true,
    });
    fs::write(
        template_manifest_path(root, &id),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut index = read_template_index(root);
    if let Some(previous) = index.insert(list.name.clone(), entry.clone()) {
        if previous.id != entry.id {
            collect_unreferenced_template_hash(root, &previous.id, &index);
        }
    }
    write_template_index(root, &index)?;
    Ok(entry)
}

/// Read the resource list of a built template by index name. Returns
/// `Ok(None)` when the name is unknown or refers to a source script
/// template (callers then fall back to the script path).
pub fn read_built_template(
    root: &str,
    name: &str,
) -> Result<Option<box_api::TemplateResourceList>, String> {
    let index = read_template_index(root);
    let Some(entry) = index.get(name) else {
        return Ok(None);
    };
    if !entry.built {
        return Ok(None);
    }
    let path = built_template_list_path(root, &entry.id);
    if !path.is_file() {
        return Err(format!(
            "built template `{name}` is corrupt: {} missing",
            path.display()
        ));
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

/// Every data-store digest referenced by any built template — the GC input
/// for `dshbox template prune`.
pub fn referenced_snapshot_digests(root: &str) -> Result<Vec<String>, String> {
    let mut digests = Vec::new();
    for entry in read_template_index(root).values() {
        if !entry.built {
            continue;
        }
        let path = built_template_list_path(root, &entry.id);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(list) = serde_json::from_str::<box_api::TemplateResourceList>(&text) else {
            continue;
        };
        for resource in &list.resources {
            if let box_api::TemplateResource::Snapshot { digest, .. } = resource {
                digests.push(digest.clone());
            }
        }
    }
    digests.sort();
    digests.dedup();
    Ok(digests)
}

pub fn installed_versions(root: &str) -> BoxResult<Vec<String>> {
    let directory = PathBuf::from(root).join("runtimes");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut versions = fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            // The completion marker (`.dshbox-runtime.json`, written only
            // after a successful clone in `pull_template`) is the installed
            // criterion — NOT the `.git` directory, which libgit2 creates
            // the instant a clone starts. Harness clones are large and take
            // minutes; keying on `.git` made the UI report "installed" while
            // the download was still running.
            is_safe_identifier(name)
                && version_directory(root, name)
                    .join(".dshbox-runtime.json")
                    .is_file()
        })
        .collect::<Vec<_>>();
    versions.sort();
    Ok(versions)
}

/// Ensure every installed DSH version has a base `.dsh` template under
/// `<root>/templates/`. The template is the canonical harness reference —
/// the Resources page surfaces it both as a "Harness" entry (for new users)
/// and as a regular template that can be extended with `ADD` lines.
///
/// Idempotent: a second run reports every flag `false`. A version that
/// fails to materialise its template is skipped without aborting the others.
pub fn upgrade_legacy_harness(root: &str) -> BoxResult<Vec<HarnessUpgradeReport>> {
    let mut reports = Vec::new();
    for version in installed_versions(root)? {
        let report = HarnessUpgradeReport {
            version: version.clone(),
            template_path: harness_template_path(root, &version)
                .to_string_lossy()
                .into_owned(),
            template_created: write_base_template(root, &version),
        };
        reports.push(report);
    }
    Ok(reports)
}

/// Returns `true` when the base template was created. Existing templates
/// are left untouched so user edits survive re-runs.
pub fn write_base_template(root: &str, version: &str) -> bool {
    let path = harness_template_path(root, version);
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = format!(
        "# DSH Box base template (auto-generated)\n\
         # Extend this script with ADD plugin|skill lines and build it from\n\
         # the Template tab.\n\
         FROM {HARNESS_STANDARD_REF}\n\
         PROFILE web\n\
         NAME {version}\n\
         VERSION latest\n"
    );
    fs::write(&path, body).is_ok()
}

/// Idempotent entry point used right after a harness install: generates the
/// base template when it does not exist yet and reports whether it was
/// created. Existing templates are never overwritten.
pub fn ensure_base_template(root: &str, version: &str) -> bool {
    write_base_template(root, version)
}

/// Pull a template by reference: clone the upstream repository into the
/// runtime directory, record the commit, and materialise the base template
/// that other containers can extend. Returns the resolved `version` slug.
///
/// This is the canonical entry point for `dshbox pull template <ref>`; it
/// replaces the older `install_dsh_version(version)` workflow. A missing
/// `:tag` / `@ref` suffix defaults to `latest`.
pub fn pull_template(
    root: &str,
    ref_value: &str,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> BoxResult<String> {
    let parsed = parse_template_ref(ref_value)?;
    let destination = version_directory(root, &parsed.version);
    if destination.exists() {
        return Err(format!(
            "template version already exists: {}",
            destination.display()
        ));
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("invalid template destination")?,
    )
    .map_err(|error| format!("cannot create template destination: {error}"))?;
    // Route the clone through the configured GitHub mirror (if any): the
    // mirror covers git transfers too, otherwise clones hit github.com
    // directly and fail with SSL errors on networks that block it.
    let mirror = read_config().ok().and_then(|config| config.github_mirror);
    let target = mirror_url(&parsed.url, mirror.as_deref());
    // `latest` is the "default branch" convention; clones resolve HEAD
    // instead of failing with `revspec 'latest' not found`. Display paths
    // (template list, version picker) keep the literal `latest` tag.
    let revision = parsed.tag.as_deref().filter(|tag| *tag != "latest");
    let commit = match shallow_clone_with_cancel(
        &target,
        &destination,
        revision,
        cancelled,
    ) {
        Ok(commit) => commit,
        Err(error) => {
            remove_checkout(&destination);
            return Err(error);
        }
    };
    fs::write(
        destination.join(".dshbox-runtime.json"),
        serde_json::json!({
            "version": parsed.version,
            "from": ref_value,
            "commit": commit,
        })
        .to_string(),
    )
    .map_err(|error| format!("cannot write runtime metadata: {error}"))?;
    // Materialise the base `<root>/templates/<ref-sanitized>.dsh` script so
    // the harness tag (e.g. `latest` or `v0.1.0`) is part of the filename
    // and two distinct refs do not overwrite each other.
    write_pulled_base_template(root, &parsed, &ref_value, &commit, destination.to_str())?;
    Ok(parsed.version)
}

/// Write the base `.dsh` script a `pull_template` produces. The script
/// body lives in the content-addressable hash directory; the index keeps
/// the user-facing ref (e.g. `github.com/deepseek-ai/deepseek-harness:latest`)
/// as the template name. A legacy alias is also written so `build_image`
/// can resolve templates by their DSH version (`latest`, `v0.1.0`) without
/// re-plumbing the build path.
fn write_pulled_base_template(
    root: &str,
    parsed: &TemplateRef,
    ref_value: &str,
    _commit: &str,
    _runtime_subdir: Option<&str>,
) -> Result<(), String> {
    let body = format!("FROM {ref_value}\nPROFILE web\nNAME {ref_value}\nVERSION latest\n");
    // Always store the template under `<ref>:<version>` so the name shown
    // by `template ls` / the UI Resources page carries the version tag
    // explicitly. A bare `github.com/<owner>/<repo>` pull still gets the
    // `:latest` suffix, matching how the harness tag is documented.
    let name = match &parsed.tag {
        Some(_tag) => ref_value.to_owned(),
        None => format!("{ref_value}:latest"),
    };
    // Always surface a non-empty `version` so the UI / CLI can render the
    // template's harness tag column. A bare pull resolves to `:latest`.
    let harness_ref = Some(parsed.tag.clone().unwrap_or_else(|| "latest".to_owned()));
    let _ = write_template_with_entry(
        root,
        &name,
        &body,
        harness_ref,
        "web",
        Some(ref_value.to_owned()),
        now_seconds(),
    )?;
    // Legacy alias kept for `build_image` compatibility: the old build
    // path reads `templates/<version>.dsh` to resolve the script body.
    let legacy = harness_template_path(root, &parsed.version);
    let _ = std::fs::write(&legacy, &body);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use box_foundation::now_seconds;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-versions-{}-{}-{name}",
            std::process::id(),
            now_seconds()
        ));
        let _ = fs::remove_dir_all(&dir);
        // A finished install carries the completion marker written by
        // `pull_template`; `.git` alone means a clone still in flight.
        let source = dir.join("runtimes/v0.1.0/source");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(
            source.join(".dshbox-runtime.json"),
            "{\"version\":\"v0.1.0\",\"commit\":\"deadbeef\"}",
        )
        .unwrap();
        dir
    }

    #[test]
    fn in_flight_clone_is_not_reported_installed() {
        let dir = fixture("in-flight");
        // Simulate a harness clone that started (`.git` created) but has
        // not finished (no marker yet): it must not count as installed,
        // otherwise the UI shows "installed" mid-download.
        let source = dir.join("runtimes/latest/source");
        fs::create_dir_all(source.join(".git")).unwrap();
        let versions = installed_versions(dir.to_str().unwrap()).unwrap();
        assert_eq!(versions, vec!["v0.1.0"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generates_base_template_for_each_installed_version() {
        let dir = fixture("generate");
        let root = dir.to_str().unwrap();
        let reports = upgrade_legacy_harness(root).unwrap();
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.version, "v0.1.0");
        assert!(report.template_created);
        assert!(report.template_path.ends_with("templates/v0.1.0.dsh"));

        let template = fs::read_to_string(dir.join("templates/v0.1.0.dsh")).unwrap();
        assert!(template.contains(&format!("FROM {HARNESS_STANDARD_REF}")));
        assert!(template.contains("PROFILE web"));
        assert!(template.contains("NAME v0.1.0"));
        assert!(!template.contains("ADD data"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = fixture("idempotent");
        let root = dir.to_str().unwrap();
        upgrade_legacy_harness(root).unwrap();
        let reports = upgrade_legacy_harness(root).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].template_created);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_template_is_not_overwritten() {
        let dir = fixture("keep-template");
        fs::create_dir_all(dir.join("templates")).unwrap();
        fs::write(dir.join("templates/v0.1.0.dsh"), "custom template body").unwrap();
        let root = dir.to_str().unwrap();
        let reports = upgrade_legacy_harness(root).unwrap();
        assert!(!reports[0].template_created);
        assert_eq!(
            fs::read_to_string(dir.join("templates/v0.1.0.dsh")).unwrap(),
            "custom template body"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_base_template_is_idempotent() {
        let dir = fixture("ensure-template");
        let root = dir.to_str().unwrap();
        assert!(ensure_base_template(root, "v0.1.0"));
        let path = dir.join("templates/v0.1.0.dsh");
        assert!(path.is_file());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("FROM github.com/deepseek-ai/deepseek-harness:latest"));
        // Second run must not overwrite user edits.
        fs::write(&path, "edited").unwrap();
        assert!(!ensure_base_template(root, "v0.1.0"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "edited");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_template_ref_defaults_to_latest() {
        let parsed = parse_template_ref("github.com/deepseek-ai/deepseek-harness").unwrap();
        assert_eq!(parsed.url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(parsed.tag, None);
        assert_eq!(parsed.version, "latest");
    }

    #[test]
    fn sanitize_template_ref_keeps_url_separators() {
        // With the hash-addressed layout the template body lives under
        // `<hash>/script.dsh`; `_sanitize_template_ref` is only kept for
        // the legacy flat-file alias so it must keep URL separators around
        // (only `\` is folded) to read more naturally.
        assert_eq!(
            sanitize_template_ref("github.com/deepseek-ai/deepseek-harness:latest"),
            "github.com/deepseek-ai/deepseek-harness:latest"
        );
        assert_eq!(
            sanitize_template_ref("  github.com/foo/bar  "),
            "github.com/foo/bar"
        );
    }

    #[test]
    fn parse_template_ref_latest_is_preserved_for_display() {
        // The parse layer keeps `latest` as the version tag so the templates
        // list and version picker show it; `pull_template` separately maps
        // `latest` to the repository HEAD before cloning.
        let parsed = parse_template_ref("github.com/deepseek-ai/deepseek-harness:latest").unwrap();
        assert_eq!(parsed.url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(parsed.tag.as_deref(), Some("latest"));
        assert_eq!(parsed.version, "latest");
    }

    #[test]
    fn parse_template_ref_accepts_colon_tag() {
        let parsed =
            parse_template_ref("github.com/deepseek-ai/deepseek-harness:v0.1.0").unwrap();
        assert_eq!(parsed.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(parsed.version, "v0.1.0");
    }

    #[test]
    fn parse_template_ref_accepts_at_ref() {
        let parsed =
            parse_template_ref("github.com/deepseek-ai/deepseek-harness@v0.1.0").unwrap();
        assert_eq!(parsed.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(parsed.version, "v0.1.0");
    }

    #[test]
    fn parse_template_ref_rejects_unknown_hosts() {
        assert!(parse_template_ref("example.com/foo/bar").is_err());
    }
}