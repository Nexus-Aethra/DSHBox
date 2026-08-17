//! Manifest schema v6 for dshimage. Every template is a flat list of
//! (source → destination) copy operations resolved against a DEF table.
//!
//! The manifest is content-addressable: each `ResolvedAdd` carries a blob
//! path and digest so the builder can skip fetches when the content already
//! exists in the archive.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::ImageError;
use crate::script::{AddKind, ImageOp, ImageScript, ParsedSource};

pub const SCHEMA_VERSION: u32 = 6;
pub const MEDIA_TYPE: &str = "application/vnd.dshbox.template.v6+json";

// ── Foundation ──────────────────────────────────────────────────────────

/// Where the template's foundation lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TemplateBase {
    /// Template carries its own runtime (harness).
    Runtime { ref_: Option<String> },
    /// Inherit another template and merge.
    Template { id: String },
}

// ── Add operations ──────────────────────────────────────────────────────

// AddKind is defined in script.rs (re-exported via lib.rs).

/// Resolved source for a single ADD operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AddSource {
    /// GitHub clone, optionally pinned.
    Github { url: String, ref_: Option<String> },
    /// URL or local tarball.
    Tarball { url: String, local: bool },
    /// Local file or directory.
    LocalPath { path: String },
    /// Path inside another plugin.
    PluginPath { plugin_name: String, rel_path: String },
    /// Path inside a container.
    ContainerPath { container_id: Option<String>, path: String },
    /// Explicit `git:` prefix form: clones a GitHub repo by short-form ref.
    GitPrefix { ref_: String },
    /// Explicit `npm:` prefix form: resolves a registry package via
    /// `pnpm pack <spec>` and imports the produced tarball.
    NpmPrefix { spec: String },
    /// Spec understood only by `box-install-handlers::parse_spec` —
    /// registry rename, `workspace:*`, `git+https://...`, `file:` /
    /// `link:` prefixes, etc. The builder forwards verbatim.
    Passthrough { spec: String },
}

/// How a resolved ADD payload is materialised into a container.
///
/// - `Code`: extension source (plugin/skill/harness). Containers share the
///   underlying files via directory links where possible.
/// - `Data`: non-code payload (datasets, media packs). Always copied to a
///   per-container directory so runtime mutations never leak across
///   containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    #[default]
    Code,
    Data,
}

/// One fully-resolved ADD operation. The builder turns each `AddOp` from
/// the script into a `ResolvedAdd` by resolving the destination against
/// the DEF table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedAdd {
    pub kind: AddKind,
    pub source: AddSource,
    /// How the payload is materialised (`code` links, `data` copies).
    #[serde(default)]
    pub resource_type: ResourceType,
    /// Fully-resolved container-relative destination path.
    pub destination: String,
    /// Archive blob path (e.g. `blobs/abc123/source`).
    pub blob: String,
    /// Content digest (fnv1a64 hex).
    pub digest: String,
}

// ── Manifest ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageManifest {
    pub schema_version: u32,
    pub media_type: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub created_at: u64,
    pub base: TemplateBase,
    /// Profile name.
    pub profile: String,
    /// Resolved DEF table (system + harness + user).
    #[serde(default)]
    pub defs: BTreeMap<String, String>,
    /// Ordered list of copy operations.
    pub adds: Vec<ResolvedAdd>,
    /// Whether data blobs are included in the archive.
    #[serde(default)]
    pub include_data: bool,
    /// Free-form labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

// ── Built-in DEF defaults ───────────────────────────────────────────────

/// System DEFs that every template inherits. User `DEF` directives can
/// override these.
fn builtin_defs(profile: &str) -> BTreeMap<String, String> {
    let mut defs = BTreeMap::new();
    defs.insert(
        "plugin".to_owned(),
        format!("@profile/profiles/{profile}/node_modules"),
    );
    defs.insert("skill".to_owned(), "@profile/skills".to_owned());
    defs.insert("data".to_owned(), "extensions/data".to_owned());
    defs
}

// ── Destination resolution ──────────────────────────────────────────────

/// Resolve a destination expression against the DEF table.
///
/// - `@plugin/foo` → DEF[plugin] + "/foo"
/// - `@skill/bar` → DEF[skill] + "/bar"
/// - `@profile/x` → literal profile path
/// - `@/abs/path` → literal absolute path
/// - bare name → error (must have @ prefix)
fn resolve_dest(dest: &str, defs: &BTreeMap<String, String>, _profile: &str) -> Result<String, ImageError> {
    let input = dest.trim();
    if !input.starts_with('@') {
        return Err(ImageError::InvalidManifest(format!(
            "destination must start with @: {input}"
        )));
    }
    let rest = &input[1..];
    // @/abs/path → literal
    if rest.starts_with('/') {
        return Ok(rest.to_owned());
    }
    // @profile/... → container profile root. The rest already includes
    // "profile/" so we use it verbatim.
    if rest.starts_with("profile/") || rest == "profile" {
        return Ok(rest.to_owned());
    }
    // @<name>/<sub> → look up DEF[name] + "/" + sub
    if let Some((name, sub)) = rest.split_once('/') {
        if let Some(base) = defs.get(name) {
            if base.is_empty() {
                return Err(ImageError::InvalidManifest(format!(
                    "DEF '{name}' has no path; specify an explicit destination"
                )));
            }
            return Ok(format!("{base}/{sub}"));
        }
        return Err(ImageError::InvalidManifest(format!(
            "unknown DEF name '{name}' in destination {input}"
        )));
    }
    // @<name> (no sub) → DEF[name] exactly
    if let Some(base) = defs.get(rest) {
        if base.is_empty() {
            return Err(ImageError::InvalidManifest(format!(
                "DEF '{rest}' has no path; specify an explicit destination"
            )));
        }
        return Ok(base.clone());
    }
    Err(ImageError::InvalidManifest(format!(
        "unknown DEF name '{rest}' in destination {input}"
    )))
}

/// Resolve the default destination for a given AddKind when no explicit
/// destination is provided.
fn default_dest(kind: AddKind, defs: &BTreeMap<String, String>, profile: &str) -> Result<String, ImageError> {
    match kind {
        AddKind::Plugin => resolve_dest("@plugin", defs, profile),
        AddKind::Skill => resolve_dest("@skill", defs, profile),
        AddKind::Data => resolve_dest("@data", defs, profile),
    }
}

// ── Source conversion ───────────────────────────────────────────────────

fn source_from_parsed(value: &ParsedSource) -> AddSource {
    match value {
        ParsedSource::Github { url, ref_ } => AddSource::Github {
            url: url.clone(),
            ref_: ref_.clone(),
        },
        ParsedSource::Tarball { url, local } => AddSource::Tarball {
            url: url.clone(),
            local: *local,
        },
        ParsedSource::LocalDir { path } => AddSource::LocalPath {
            path: path.to_string_lossy().into_owned(),
        },
        ParsedSource::BareName { name, scope, version: _ } => {
            let full_name = match scope {
                Some(scope) => format!("@{scope}/{name}"),
                None => name.clone(),
            };
            AddSource::LocalPath { path: full_name }
        }
        ParsedSource::Passthrough { spec } => {
            // Round-trip via the install-handlers crate so manifest
            // serialisation sees the canonical InstallSpec shape; the
            // existing AddSource enum does not model workspace/alias,
            // so we re-encode as a tagged marker the builder can match.
            AddSource::Passthrough { spec: spec.clone() }
        }
        ParsedSource::GitPrefix { ref_ } => AddSource::GitPrefix { ref_: ref_.clone() },
        ParsedSource::NpmPrefix { spec } => AddSource::NpmPrefix { spec: spec.clone() },
    }
}

fn source_name(source: &ParsedSource) -> String {
    match source {
        ParsedSource::Github { url, .. } => {
            let trimmed = url.trim_end_matches('/');
            let without_scheme = trimmed
                .strip_prefix("https://")
                .or_else(|| trimmed.strip_prefix("http://"))
                .unwrap_or(trimmed);
            without_scheme.rsplit('/').next().unwrap_or("").trim_end_matches(".git").to_string()
        }
        ParsedSource::Tarball { url, .. } => {
            let path = url.split('?').next().unwrap_or(url);
            let last = path.rsplit('/').next().unwrap_or(path);
            last.rsplit_once('.').map(|(head, _)| head).unwrap_or(last).to_string()
        }
        ParsedSource::LocalDir { path } => path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("extension")
            .to_string(),
        ParsedSource::BareName { name, scope, .. } => match scope {
            Some(scope) => format!("@{scope}/{name}"),
            None => name.clone(),
        },
        ParsedSource::GitPrefix { ref_ } => {
            // `git:github.com/owner/repo:tag` → derive name from the last
            // short-form segment, mirroring the implicit shape.
            let last_at = ref_.rfind('@');
            let last_colon = ref_.rfind(':');
            let cut = match (last_at, last_colon) {
                (Some(at), Some(colon)) if at > colon => at,
                (Some(_), Some(colon)) => colon,
                (Some(at), None) => at,
                (None, Some(colon)) => colon,
                (None, None) => return "git-extension".to_owned(),
            };
            let head = &ref_[..cut];
            head.rsplit('/').next().unwrap_or("git-extension").trim_end_matches(".git").to_string()
        }
        ParsedSource::NpmPrefix { spec } => {
            // `npm:@scope/name@1.2.3` → `@scope/name`; `npm:name@1.2.3` → `name`.
            let stripped_at = spec.strip_prefix('@').unwrap_or(spec);
            let slash = stripped_at.find('/');
            let (scope_part, rest) = match slash {
                Some(pos) => (Some(&spec[..pos + 1]), &stripped_at[pos + 1..]),
                None => (None, stripped_at),
            };
            let version_at = rest.rfind('@');
            let name = match version_at {
                Some(pos) => &rest[..pos],
                None => rest,
            };
            match scope_part {
                Some(scope) => format!("{scope}{name}"),
                None => name.to_owned(),
            }
        }
        ParsedSource::Passthrough { spec } => spec.clone(),
    }
}

fn digest_of(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

// ── Compilation ─────────────────────────────────────────────────────────

/// Compile an `ImageScript` into a v6 manifest.
pub fn compile_manifest(script: &ImageScript, created_at: u64) -> ImageManifest {
    // Start with built-in DEFs, then overlay harness DEFs, then user DEFs.
    let mut defs = builtin_defs(&script.profile);
    for (name, path) in &script.defs {
        defs.insert(name.clone(), path.clone());
    }

    let mut adds = Vec::new();

    // Harness itself is always the first add.
    let harness_digest = script.harness_digest();
    adds.push(ResolvedAdd {
        kind: AddKind::Plugin,
        source: AddSource::Github {
            url: script.harness_url.clone(),
            ref_: script.harness_ref.clone(),
        },
        resource_type: ResourceType::Code,
        destination: format!("@runtime/dsh/{}", script.harness_ref.as_deref().unwrap_or("latest")),
        blob: format!("blobs/harness/{harness_digest}"),
        digest: harness_digest,
    });

    // Each ADD op.
    for op in &script.ops {
        match op {
            ImageOp::Add { kind, source, .. } => {
                let add_kind = *kind;
                let dest = default_dest(add_kind, &defs, &script.profile).unwrap_or_default();
                let src_name = source_name(source);
                let name_for_digest = format!("{}:{}", src_name, dest);
                let dig = digest_of(&name_for_digest);
                adds.push(ResolvedAdd {
                    kind: add_kind,
                    source: source_from_parsed(source),
                    resource_type: match add_kind {
                        AddKind::Data => ResourceType::Data,
                        AddKind::Plugin | AddKind::Skill => ResourceType::Code,
                    },
                    destination: format!("{dest}/{src_name}"),
                    blob: format!("blobs/{dig}"),
                    digest: dig,
                });
            }
        }
    }

    ImageManifest {
        schema_version: SCHEMA_VERSION,
        media_type: MEDIA_TYPE.to_string(),
        id: format!("{}:{}", script.name, script.version),
        name: script.name.clone(),
        version: script.version.clone(),
        created_at,
        base: TemplateBase::Runtime {
            ref_: script.harness_ref.clone(),
        },
        profile: script.profile.clone(),
        defs,
        adds,
        include_data: false,
        labels: script.labels.clone(),
    }
}

// ── I/O ─────────────────────────────────────────────────────────────────

pub fn parse_manifest(json: &str) -> Result<ImageManifest, ImageError> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let manifest: ImageManifest = serde_json::from_value(value.clone())
        .map_err(|error| ImageError::InvalidManifest(error.to_string()))?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(ImageError::InvalidManifest(format!(
            "unsupported schemaVersion {}, expected {SCHEMA_VERSION}",
            manifest.schema_version
        )));
    }
    if manifest.media_type != MEDIA_TYPE {
        return Err(ImageError::InvalidManifest(format!(
            "unexpected mediaType `{}`",
            manifest.media_type
        )));
    }
    Ok(manifest)
}

pub fn serialize_manifest(manifest: &ImageManifest) -> Result<String, ImageError> {
    Ok(serde_json::to_string_pretty(manifest)?)
}

pub fn write_manifest_to(path: &Path, manifest: &ImageManifest) -> Result<(), ImageError> {
    let text = serialize_manifest(manifest)?;
    std::fs::write(path, text)?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::parse_script;
    use std::path::PathBuf;

    #[test]
    fn round_trip_v6_manifest() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             NAME team-stack
             VERSION 1.2.3
             ADD plugin github.com/team/foo:1.2.3
             ADD plugin cordis-plugin-bar
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        let json = serialize_manifest(&manifest).unwrap();
        let restored = parse_manifest(&json).unwrap();
        assert_eq!(restored, manifest);
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let json = r#"{
            "schemaVersion": 99,
            "mediaType": "application/vnd.dshbox.template.v6+json",
            "id": "x:1",
            "name": "x",
            "version": "1",
            "createdAt": 0,
            "base": { "type": "runtime" },
            "profile": "web",
            "adds": []
        }"#;
        assert!(parse_manifest(json).is_err());
    }

    #[test]
    fn harness_is_first_add() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        assert!(!manifest.adds.is_empty());
        let first = &manifest.adds[0];
        assert!(matches!(first.source, AddSource::Github { .. }));
    }

    #[test]
    fn user_plugins_are_adds() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             ADD plugin cordis-plugin-foo
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 1 plugin = 2 adds
        assert_eq!(manifest.adds.len(), 2);
        let plugin = &manifest.adds[1];
        assert_eq!(plugin.kind, AddKind::Plugin);
        assert_eq!(plugin.resource_type, ResourceType::Code);
        assert!(plugin.destination.contains("cordis-plugin-foo"));
    }

    #[test]
    fn data_adds_are_marked_as_data_resources() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\n             PROFILE web\n             ADD data team-docs\n             ADD plugin cordis-plugin-foo\n",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 1 data + 1 plugin = 3 adds
        assert_eq!(manifest.adds.len(), 3);
        let data = &manifest.adds[1];
        assert_eq!(data.kind, AddKind::Data);
        assert_eq!(data.resource_type, ResourceType::Data);
        assert!(data.destination.starts_with("extensions/data/"));
        let plugin = &manifest.adds[2];
        assert_eq!(plugin.resource_type, ResourceType::Code);
    }

    #[test]
    fn duplicate_plugins_are_separate_adds() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             ADD plugin cordis-plugin-foo@1.0.0
             ADD plugin cordis-plugin-foo@2.0.0
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 2 plugins = 3 adds (duplicates are separate layers)
        assert_eq!(manifest.adds.len(), 3);
    }

    #[test]
    fn builtin_defs_are_present() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        assert!(manifest.defs.contains_key("plugin"));
        assert!(manifest.defs.contains_key("skill"));
        assert!(manifest.defs.contains_key("data"));
    }
}
