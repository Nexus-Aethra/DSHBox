//! Wire types shared between `dshboxd` and every client (desktop Tauri
//! commands, the `dshbox` CLI).
//!
//! Before this crate existed, the daemon and the desktop layer each kept a
//! hand-copied definition of these structs; a field added on one side
//! (e.g. the hash-storage `id` replacing `path`) silently turned a
//! successful RPC into a deserialization error and the UI showed an empty
//! list while the CLI kept working. With a single definition, any drift is
//! a compile error, and the fixture tests below pin the JSON shape so a
//! rename cannot sneak past unnoticed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One local template as returned by `list_templates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub name: String,
    /// Content-addressable identifier (fnv1a64 hex of the script body or
    /// the built resource list).
    pub id: String,
    pub harness_ref: Option<String>,
    pub profile: String,
    /// True for built templates (resource list form); false for source
    /// script templates. Absent on older payloads, hence the default.
    #[serde(default)]
    pub built: bool,
}

/// Raw text of a template as returned by `read_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateText {
    pub name: String,
    pub text: String,
}

/// Parameters of the `build_image` task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildImageRequest {
    pub script_path: String,
    pub output_path: Option<String>,
    pub container_name: Option<String>,
}

/// Parameters of `create_container_from_template`. The template may be a
/// source script template or a built template (resource list); the daemon
/// resolves which form it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTemplateContainerRequest {
    pub name: String,
    pub template: String,
    pub profile: Option<String>,
}

/// Parameters of `import_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTemplateRequest {
    pub archive: String,
    pub name: Option<String>,
}

/// Parameters of `export_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTemplateRequest {
    pub name: String,
    pub destination: Option<String>,
}

/// Parameters of `remove_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveTemplateRequest {
    pub name: String,
}

// ── Built-template resource list ──────────────────────────────────────
// DSH Box has a single unit of construction — the template. A *built*
// template is the fully physical product of `dshbox build` (spec:
// docs/specs/image-build.md). Every resource row points at a file or
// directory inside the template's own storage tree (the hash-addressed
// directory under `<root>/templates/<id>/`). Container materialisation
// `cp -rL`s each row into the destination path verbatim — there is no
// shared-plugin-repository indirection at this layer. There is no
// separate "image" concept; the word survives only as a deprecated CLI
// alias.

/// Bump together with any structural change to [`TemplateResourceList`].
pub const TEMPLATE_LIST_SCHEMA_VERSION: u32 = 8;

/// One resource row inside a built template.
///
/// `source_kind` distinguishes how the row's `source` (a path string)
/// should be interpreted:
///   - `"plugin"` — a `<scope>/<name>` subdirectory of the template's
///     `repository/` tree (legacy rows may carry the historical
///     repository entry id; readers accept both).
///   - `"skill"` / `"data"` — a content-addressed snapshot under the
///     template's `data/<digest>/` tree.
///
/// `sha256` is the content hash of the resource at build time, used by
/// consumers to detect drift when the same template is re-applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateResource {
    /// `"plugin" | "skill" | "data"`.
    pub kind: String,
    /// Plugin/skill/data name (the canonical npm name or skill folder).
    pub name: String,
    /// Origin discriminator; see the schema doc above.
    pub source_kind: String,
    /// Path the materialiser should copy out of the template directory.
    /// Plugin: `repository/<scope>/<name>` (relative). Skill/data:
    /// `data/<digest>` (relative). Older `reference` rows used an absolute
    /// runtime path; the reader accepts those for backward compatibility,
    /// but newer writes always emit the relative form.
    pub source: String,
    /// Where the resource lands inside the container profile
    /// (`profile/skills/<name>` or `node_modules/<scope>/<name>`).
    pub destination: String,
    /// SHA-256 of the resource file/directory content. Hex, lowercase.
    pub sha256: String,
}

/// The complete metadata of one built template — the only thing it owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateResourceList {
    pub schema_version: u32,
    pub name: String,
    /// The template (or harness ref) this built template derives from.
    pub base: String,
    pub profile: String,
    pub harness_ref: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub created_at: u64,
    pub resources: Vec<TemplateResource>,
}

/// Stub payload for the planned `image commit` flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitImageRequest {
    pub container_id: String,
    pub output_path: String,
    pub name: String,
    pub version: String,
}

/// Stub payload for the planned `image load` flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadImageRequest {
    pub archive_path: String,
}

/// One entry of the content-addressed data store. Doubles as the on-disk
/// index format (`<root>/data/index.json`), so treat field renames as a
/// storage migration, not just a wire change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataEntry {
    pub name: String,
    pub digest: String,
    pub imported_at: u64,
    pub source: String,
}

/// One data payload copied into a container; recorded in
/// `<container>/state/data.json` so `image prune` knows which digests are
/// still in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataUse {
    pub name: String,
    pub digest: String,
}

/// Detailed view of one container, returned by the `describe_container` RPC.
/// Combines the on-disk `DshContainer` summary with live runtime signals
/// (URL, host PID) and a full extensions scan (profiles, plugins, skills).
/// The CLI's `container describe` renders this in two modes — a human
/// text view by default and raw JSON via `--json` — while the desktop
/// layer can later reuse the same payload for its details panel.
///
/// The `extensions` field is carried as a raw `serde_json::Value` so this
/// wire crate stays a leaf: it does not need to mirror the deep
/// `ContainerExtensions` shape from `box-extensions`, and the daemon can
/// serialise whatever it scans without forcing a new dep here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerDescription {
    pub id: String,
    pub name: String,
    pub version: String,
    pub profile: String,
    /// The built/script template the container was materialised from
    /// (`container.json["template"]`); `None` for legacy image-based
    /// containers where only the `image` alias exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub directory: String,
    /// "running" / "stopped" — same vocabulary as `DshContainer::status`.
    pub status: String,
    /// Execution backend the container's record requests, normalised by the
    /// daemon. Three cases: a record with no `runtime` field — and a record
    /// carrying an unparseable value, since this read verb must not fail —
    /// answers `"host"`; `"host"` answers `"host"`; a `vm[:backend]` value
    /// answers its canonical lowercase `"vm:backend"`. Enforcement happens
    /// at start, where an unexecutable backend is an error.
    #[serde(default = "default_container_runtime")]
    pub runtime: String,
    /// Webview URL the daemon is serving; `Some` only while `status` is
    /// "running".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// PID of the DSH host process; `Some` only while the PID file is
    /// present and `kill -0` (or `tasklist` on Windows) confirms the
    /// process is still alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    /// Per-profile plugin / skill scan; covers every DSH profile the
    /// container was materialised with. Carried as a generic JSON object
    /// — the full `ContainerExtensions` struct lives in `box-extensions`,
    /// which cannot be a dep of this leaf wire crate (it would create a
    /// cycle via `box-containers`).
    pub extensions: serde_json::Value,
}

fn default_container_runtime() -> String {
    "host".to_owned()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
