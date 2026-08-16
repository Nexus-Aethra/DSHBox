use super::*;
use box_dsh_versions::HARNESS_STANDARD_REF;

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    is_safe_identifier(version)
}

/// Queue a template pull. The `version` is the version slug (the `:tag`
/// half of the template reference); the desktop fills in the GitHub base
/// from the canonical harness reference. The daemon resolves the rest,
/// including the missing-tag default of `latest`.
#[tauri::command]
pub(crate) fn enqueue_pull_template(
    version: String,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if version.is_empty() {
        return Err("expected a DSH version tag".to_owned());
    }
    let base = HARNESS_STANDARD_REF
        .rsplit_once(':')
        .map(|(head, _)| head)
        .unwrap_or(HARNESS_STANDARD_REF);
    let ref_value = format!("{base}:{version}");
    let client = connect()?;
    let value = call(
        &client,
        "pull_template",
        serde_json::json!({ "ref": ref_value }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_dsh_catalog_refresh(
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let client = connect()?;
    let value = call(&client, "refresh_dsh_catalog", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}
