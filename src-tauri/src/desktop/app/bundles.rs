use super::*;

#[tauri::command]
pub(crate) fn list_extension_bundles() -> Result<Vec<ExtensionBundle>, String> {
    let client = connect()?;
    let value = call(&client, "list_bundles", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid bundle list: {error}"))
}

#[tauri::command]
pub(crate) fn create_extension_bundle(
    name: String,
    repository_ids: Vec<String>,
) -> Result<ExtensionBundle, String> {
    let name = name.trim();
    if name.is_empty() || repository_ids.is_empty() {
        return Err("bundle needs a name and at least one extension".to_owned());
    }
    if repository_ids.len() > 32 {
        return Err("bundle supports at most 32 extensions".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "create_extension_bundle",
        serde_json::json!({ "name": name, "repositoryIds": repository_ids }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid bundle record: {error}"))
}

#[tauri::command]
pub(crate) fn delete_extension_bundle(id: String) -> Result<(), String> {
    let client = connect()?;
    call(&client, "delete_extension_bundle", serde_json::json!({ "id": id }))?;
    Ok(())
}

#[tauri::command]
pub(crate) fn enqueue_bundle_export(
    id: String,
    destination: String,
    mode: String,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&id)
        || destination.trim().is_empty()
        || !matches!(mode.as_str(), "quick" | "full")
    {
        return Err("invalid bundle export request".to_owned());
    }
    if PathBuf::from(&destination).extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("bundle export destination must end in .tar.gz".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "export_bundle",
        serde_json::json!({
            "bundleId": id,
            "destination": absolutize_path(&destination),
            "mode": mode,
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[derive(Deserialize)]
pub(crate) struct ImportBundleRequest {
    pub(crate) archive: String,
    /// "overwrite" replaces same-named repository entries; "keep" keeps the
    /// existing entry and gives the imported one a "name (2)" suffix.
    pub(crate) conflict: String,
}

#[tauri::command]
pub(crate) fn enqueue_bundle_import(
    request: ImportBundleRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if request.archive.trim().is_empty()
        || !matches!(request.conflict.as_str(), "overwrite" | "keep")
    {
        return Err("invalid bundle import request".to_owned());
    }
    let archive = PathBuf::from(&request.archive);
    if !archive.is_file() {
        return Err("bundle archive must be an existing local file".to_owned());
    }
    if archive.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("bundle import source must end in .tar.gz".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "import_bundle",
        serde_json::json!({
            "archive": absolutize_path(&request.archive),
            "conflict": request.conflict,
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[derive(Deserialize)]
pub(crate) struct InstallBundleRequest {
    /// Container id.
    id: String,
    profile: String,
    bundle_id: String,
    /// "overwrite" re-installs same-named plugins/skills; "keep" leaves them.
    conflict: String,
}

#[tauri::command]
pub(crate) fn enqueue_container_bundle_install(
    request: InstallBundleRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id)
        || !is_safe_identifier(&request.profile)
        || !is_safe_identifier(&request.bundle_id)
        || !matches!(request.conflict.as_str(), "overwrite" | "keep")
    {
        return Err("invalid bundle install request".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_bundle_install",
        serde_json::json!({
            "id": request.id,
            "profile": request.profile,
            "bundleId": request.bundle_id,
            "conflict": request.conflict,
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}
