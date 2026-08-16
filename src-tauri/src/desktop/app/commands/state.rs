use super::super::*;

/// Returns the complete application read model for lists and diagnostics.
#[tauri::command]
pub(crate) fn list_resource_states(
    resources: tauri::State<ResourceStateManager>,
) -> Result<ResourceSnapshot, String> {
    resources.snapshot()
}

/// Returns one resource addressed by its scheduler-compatible key.
#[tauri::command]
pub(crate) fn get_resource_state(
    key: String,
    resources: tauri::State<ResourceStateManager>,
) -> Result<Option<ResourceState>, String> {
    resources.resource(&key)
}

/// Returns cached, container-owned profiles, enabled plugins, and skills.
#[tauri::command]
pub(crate) fn get_container_details(
    id: String,
    resources: tauri::State<ResourceStateManager>,
) -> Result<Option<box_extensions::ContainerExtensions>, String> {
    resources.container_extensions(&id)
}

/// Re-scans real resources and updates the process-local read model.
#[tauri::command]
pub(crate) fn refresh_resource_state(app: tauri::AppHandle) -> Result<ResourceSnapshot, String> {
    refresh_global_state(&app);
    app.state::<ResourceStateManager>().snapshot()
}

/// One entry of the content-addressed data store (image tab). Shared
/// through box-api with the daemon that serializes it.
pub(crate) use box_api::DataEntry;

/// Lists data-store entries via the daemon.
#[tauri::command]
pub(crate) fn list_data_entries() -> Result<Vec<DataEntry>, String> {
    let client = connect()?;
    let value = call(&client, "list_data_entries", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid data entries from daemon: {error}"))
}

/// Garbage-collects orphaned data-store blobs via the daemon. Returns the
/// removed content digests.
#[tauri::command]
pub(crate) fn prune_orphaned_data() -> Result<Vec<String>, String> {
    let client = connect()?;
    let value = call(&client, "prune_orphaned_data", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid prune response from daemon: {error}"))
}
