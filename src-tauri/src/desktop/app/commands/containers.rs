use super::super::*;

#[tauri::command]
pub(crate) fn list_dsh_containers(
    _resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<DshContainer>, String> {
    let client = connect()?;
    let value = call(&client, "list_containers", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid container list: {error}"))
}
