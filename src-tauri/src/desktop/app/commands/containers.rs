use super::super::*;

#[tauri::command]
pub(crate) fn list_dsh_containers(
    resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<DshContainer>, String> {
    Ok(resources.snapshot()?.containers)
}
