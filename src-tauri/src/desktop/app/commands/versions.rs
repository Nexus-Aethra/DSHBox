use super::super::*;
use box_dsh_versions::DshVersion;

#[tauri::command]
pub(crate) fn list_dsh_versions(
    resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<DshVersion>, String> {
    let client = connect()?;
    // The daemon derives the Harness tab list directly from the template
    // index — no separate catalog file is read or maintained. The payload
    // carries both `name` and `installed`, so the UI no longer needs a
    // second round-trip to `list_installed_dsh_versions`.
    let versions: Vec<DshVersion> = serde_json::from_value(call(
        &client,
        "list_dsh_catalog",
        serde_json::json!({}),
    )?)
    .map_err(|error| format!("invalid version catalog: {error}"))?;
    for version in &versions {
        resources.refresh_runtime(version.clone());
    }
    Ok(versions)
}

#[tauri::command]
pub(crate) fn upgrade_legacy_resources(
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<Vec<String>, String> {
    // One-shot migration: mirror every `<runtime>/runtimes/<tag>/source/`
    // that lacks a template index entry. Returns the tags registered in
    // this pass so the UI can display a confirmation.
    let client = connect()?;
    let value = call(&client, "upgrade_legacy_resources", serde_json::json!({}))?;
    let registered = value
        .get("registered")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(registered)
}

#[tauri::command]
pub(crate) fn list_installed_dsh_versions(
    _resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<String>, String> {
    let client = connect()?;
    let value = call(&client, "list_installed_dsh_versions", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid installed versions: {error}"))
}

#[tauri::command]
pub(crate) fn uninstall_dsh_version(
    version: String,
    _manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    let client = connect()?;
    call(
        &client,
        "uninstall_dsh_version",
        serde_json::json!({ "version": version }),
    )?;
    let config = read_config()?;
    refresh_global_state(&app);
    Ok(config)
}