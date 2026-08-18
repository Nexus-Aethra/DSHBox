use super::*;

pub(crate) fn is_safe_package_name(name: &str) -> bool {
    !name.is_empty() && !name.contains("..") && name.split('/').all(is_safe_identifier)
}

pub(crate) fn is_safe_workspace_relative_path(value: &str) -> bool {
    !value.is_empty() && !Path::new(value).is_absolute() && !Path::new(value).components().any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)))
}

#[tauri::command]
pub(crate) fn enqueue_container_extension_add(
    request: AddContainerExtensionRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_identifier(&request.profile) {
        return Err("invalid container or profile name".to_owned());
    }
    let source = request.source.trim().to_owned();
    if source.is_empty() {
        return Err("extension source is required".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_extension_add",
        serde_json::json!({
            "id": request.id,
            "profile": request.profile,
            "source": absolutize_path(&source),
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_repository_extension_import(
    request: ImportRepositoryExtensionRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if request.source.trim().is_empty() { return Err("extension source is required".to_owned()); }
    let client = connect()?;
    let value = call(
        &client,
        "import_repository_extension",
        serde_json::json!({ "source": absolutize_path(&request.source) }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_repository_extension_export(
    request: ExportRepositoryExtensionRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.repository_id) || request.destination.trim().is_empty() { return Err("invalid extension export request".to_owned()); }
    let client = connect()?;
    let value = call(
        &client,
        "export_repository_extension",
        serde_json::json!({
            "repositoryId": request.repository_id,
            "destination": absolutize_path(&request.destination),
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_plugin_export(
    request: ExportContainerPluginRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.source_container_id)
        || request.source_path.trim().is_empty()
        || request.destination.trim().is_empty()
    {
        return Err("invalid plugin export request".to_owned());
    }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_plugin_export",
        serde_json::json!({
            "sourceContainerId": request.source_container_id,
            "sourcePath": absolutize_path(&request.source_path),
            "destination": absolutize_path(&request.destination),
        }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn scan_container_workspace_extensions(id: String) -> Result<Vec<box_extensions::WorkspaceExtension>, String> {
    if !is_safe_identifier(&id) { return Err("invalid container id".to_owned()); }
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let container = scan_containers(&root)?.remove(&id).ok_or("container not found")?;
    Ok(scan_workspace_extensions(&PathBuf::from(container.directory).join("workspace")))
}

#[tauri::command]
pub(crate) fn enqueue_workspace_extension_import(
    request: ImportWorkspaceExtensionRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_workspace_relative_path(&request.relative_path) { return Err("invalid workspace extension path".to_owned()); }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_workspace_extension_import",
        serde_json::json!({ "id": request.id, "relativePath": request.relative_path }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_container_extension_copy(
    request: CopyRepositoryExtensionRequest,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_identifier(&request.repository_id) || request.profile.as_deref().is_some_and(|value| !is_safe_identifier(value)) { return Err("invalid extension copy request".to_owned()); }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_extension_copy",
        serde_json::json!({ "id": request.id, "profile": request.profile, "repositoryId": request.repository_id }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn remove_repository_extension(id: String, _tasks: tauri::State<TaskManager>, app: tauri::AppHandle) -> Result<(), String> {
    if !is_safe_identifier(&id) { return Err("invalid repository extension id".to_owned()); }
    let client = connect()?;
    call(&client, "remove_repository_extension", serde_json::json!({ "id": id }))?;
    refresh_global_state(&app);
    Ok(())
}

/// Debugging aid exposed as a tauri command so the resources page can
/// show a popover of owner ids alongside the snapshot counts.
#[tauri::command]
pub(crate) fn list_repository_reference_counts() -> Result<Vec<box_extensions::RepositoryReferenceRow>, String> {
    let client = connect()?;
    let value = call(&client, "list_repository_reference_counts", serde_json::json!({}))?;
    serde_json::from_value(value).map_err(|error| format!("invalid reference counts response: {error}"))
}

#[tauri::command]
pub(crate) fn remove_repository_plugin(
    id: String,
    profile: String,
    name: String,
    _tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) || !is_safe_package_name(&name) {
        return Err("invalid plugin removal request".to_owned());
    }
    let client = connect()?;
    call(
        &client,
        "remove_repository_plugin",
        serde_json::json!({ "id": id, "profile": profile, "name": name }),
    )?;
    refresh_global_state(&app);
    Ok(())
}
