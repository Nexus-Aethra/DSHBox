//! dshimage build orchestration: parse a `.dsh` script, materialise each
//! ADD into the Repository, install into the freshly-created container, and
//! (optionally) write a portable `.dshimage` archive.
//!
//! The crate `box-image` owns the format and the parser. This module is the
//! host-side glue: it knows about Repository entries, container profiles,
//! task scheduling, and the archive layout.
//!
//! Builds run on the daemon now; this module keeps the script preview (pure
//! parsing) and the commit/load stubs local.

use std::path::Path;

use box_api::{
    BuildImageRequest, CommitImageRequest, CreateTemplateContainerRequest,
    ExportTemplateRequest, ImportTemplateRequest, LoadImageRequest, RemoveTemplateRequest,
    TemplateInfo, TemplateText,
};
use box_image::{
    parse_manifest, parse_script, ImageManifest, ImageOp, ParsedSource,
};
use box_scheduler::{TaskManager, TaskRecord};
use tauri::AppHandle;

use super::{absolutize_path, call, connect, queue_task};

// Wire structs (TemplateInfo, BuildImageRequest, ...) come from box-api so
// the desktop passthrough deserializes the exact shape the daemon
// serializes; drift is a compile error now instead of a silent empty list.

/// Frontend-friendly preview result.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewScriptResult {
    pub name: String,
    pub version: String,
    pub harness_url: String,
    pub profile: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub ops: Vec<PreviewOp>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewOp {
    pub kind: String,
    pub line: usize,
    pub source: String,
    pub parsed: PreviewSource,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum PreviewSource {
    Github { url: String, ref_: Option<String> },
    Tarball { url: String, local: bool },
    LocalDir { path: String },
    BareName {
        name: String,
        scope: Option<String>,
        version: Option<String>,
    },
}

impl PreviewSource {
    fn from_parsed(value: &ParsedSource) -> Self {
        match value {
            ParsedSource::Github { url, ref_ } => PreviewSource::Github {
                url: url.clone(),
                ref_: ref_.clone(),
            },
            ParsedSource::Tarball { url, local } => PreviewSource::Tarball {
                url: url.clone(),
                local: *local,
            },
            ParsedSource::LocalDir { path } => PreviewSource::LocalDir {
                path: path.to_string_lossy().into_owned(),
            },
            ParsedSource::BareName { name, scope, version } => PreviewSource::BareName {
                name: name.clone(),
                scope: scope.clone(),
                version: version.clone(),
            },
        }
    }
}

/// Parse a script and return a frontend-friendly preview, without doing
/// any I/O beyond reading the script file itself.
pub(crate) fn preview_image_script(path: &Path) -> Result<PreviewScriptResult, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read script `{}`: {error}", path.display()))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let script = parse_script(&text, base_dir)
        .map_err(|error| format!("script parse error: {error}"))?;
    let mut ops = Vec::new();
    for op in &script.ops {
        match op {
            ImageOp::Add { kind, source, line } => ops.push(PreviewOp {
                kind: format!("{:?}", kind).to_lowercase(),
                line: *line,
                source: describe_parsed(source),
                parsed: PreviewSource::from_parsed(source),
            }),
        }
    }
    Ok(PreviewScriptResult {
        name: script.name.clone(),
        version: script.version.clone(),
        harness_url: script.harness_url.clone(),
        profile: script.profile.clone(),
        labels: script.labels.clone(),
        ops,
    })
}

fn describe_parsed(value: &ParsedSource) -> String {
    match value {
        ParsedSource::Github { url, ref_ } => match ref_ {
            Some(reference) => format!("{url}@{reference}"),
            None => url.clone(),
        },
        ParsedSource::Tarball { url, .. } => url.clone(),
        ParsedSource::LocalDir { path } => path.to_string_lossy().into_owned(),
        ParsedSource::BareName { name, scope, version } => {
            let head = match scope {
                Some(scope) => format!("@{scope}/{name}"),
                None => name.clone(),
            };
            match version {
                Some(version) => format!("{head}@{version}"),
                None => head,
            }
        }
    }
}

#[tauri::command]
pub(crate) fn preview_image_script_command(path: String) -> Result<PreviewScriptResult, String> {
    preview_image_script(Path::new(&path))
}

#[tauri::command]
pub(crate) fn enqueue_image_build(
    request: BuildImageRequest,
    _manager: tauri::State<TaskManager>,
    _app: AppHandle,
) -> Result<TaskRecord, String> {
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_build",
        serde_json::json!({
            "scriptPath": absolutize_path(&request.script_path),
            "outputPath": request
                .output_path
                .map(|path| absolutize_path(&path)),
            "containerName": request.container_name,
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

/// List the local templates (`<root>/templates/*.dsh`) with their harness
/// ref and profile so the UI can offer them when creating containers.
#[tauri::command]
pub(crate) fn list_templates() -> Result<Vec<TemplateInfo>, String> {
    let client = connect()?;
    let value = call(&client, "list_templates", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid template list: {error}"))
}

/// Read the raw text of a local template so the UI can preview it before
/// building or exporting.
#[tauri::command]
pub(crate) fn read_template(name: String) -> Result<TemplateText, String> {
    let client = connect()?;
    let value = call(&client, "read_template", serde_json::json!({ "name": name }))?;
    let text = value["text"]
        .as_str()
        .ok_or_else(|| "invalid read_template response from daemon".to_owned())?
        .to_owned();
    let name = value["name"].as_str().unwrap_or("").to_owned();
    Ok(TemplateText { name, text })
}

/// Import a template archive (the same `.dsh.tar.gz` shape `export_template`
/// writes) and add it to `<root>/templates/`.
#[tauri::command]
pub(crate) fn import_template(request: ImportTemplateRequest) -> Result<String, String> {
    let client = connect()?;
    let mut params = serde_json::json!({
        "archive": absolutize_path(&request.archive),
    });
    if let Some(name) = request.name.filter(|value| !value.is_empty()) {
        params["name"] = serde_json::json!(name);
    }
    let value = call(&client, "import_template", params)?;
    let name = value["name"]
        .as_str()
        .ok_or_else(|| "invalid import_template response from daemon".to_owned())?
        .to_owned();
    Ok(name)
}

/// Export a local template to a gzip tarball at the given destination
/// (default: `./<name>.dsh.tar.gz`).
#[tauri::command]
pub(crate) fn export_template(request: ExportTemplateRequest) -> Result<String, String> {
    let client = connect()?;
    let mut params = serde_json::json!({ "name": request.name });
    if let Some(destination) = request.destination.filter(|value| !value.is_empty()) {
        params["destination"] = serde_json::json!(absolutize_path(&destination));
    }
    let value = call(&client, "export_template", params)?;
    let path = value["path"]
        .as_str()
        .ok_or_else(|| "invalid export_template response from daemon".to_owned())?
        .to_owned();
    Ok(path)
}

/// Remove a local template. Refuses if any container still references it
/// (mirrors the reference-counting guard the plugin resource uses).
#[tauri::command]
pub(crate) fn remove_template(request: RemoveTemplateRequest) -> Result<String, String> {
    let client = connect()?;
    let value = call(&client, "remove_template", serde_json::json!({ "name": request.name }))?;
    let name = value["name"]
        .as_str()
        .unwrap_or(&request.name)
        .to_owned();
    Ok(name)
}

#[tauri::command]
pub(crate) fn enqueue_template_container(
    request: CreateTemplateContainerRequest,
    _manager: tauri::State<TaskManager>,
    _app: AppHandle,
) -> Result<TaskRecord, String> {
    let client = connect()?;
    let value = call(
        &client,
        "create_container_from_template",
        serde_json::json!({
            "name": request.name,
            "template": request.template,
            "profile": request.profile,
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_image_commit_stub(
    _request: CommitImageRequest,
    manager: tauri::State<TaskManager>,
    app: AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "image-commit",
        vec!["repository:extensions".to_owned()],
        serde_json::json!({"reason": "not yet implemented"}),
    )?;
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_image_load_stub(
    _request: LoadImageRequest,
    manager: tauri::State<TaskManager>,
    app: AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "image-load",
        vec!["repository:extensions".to_owned()],
        serde_json::json!({"reason": "not yet implemented"}),
    )?;
    Ok(task)
}

#[allow(dead_code)]
pub(crate) fn validate_archive(path: &Path) -> Result<ImageManifest, String> {
    let staging = std::env::temp_dir().join(format!(
        "dshbox-image-inspect-{}",
        std::process::id()
    ));
    let archive = box_image::read_dshimage(path, &staging)
        .map_err(|error| format!("cannot read archive: {error}"))?;
    let manifest = archive.manifest;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(manifest)
}

#[allow(dead_code)]
pub(crate) fn parse_manifest_text(text: &str) -> Result<ImageManifest, String> {
    parse_manifest(text).map_err(|error| format!("{error}"))
}

#[allow(dead_code)]
pub(crate) fn serialize_manifest_text(manifest: &ImageManifest) -> Result<String, String> {
    box_image::serialize_manifest(manifest).map_err(|error| format!("{error}"))
}

