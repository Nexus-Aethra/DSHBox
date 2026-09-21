use super::*;

/// The resource layer's read models are built inline by the daemon (`json!`
/// over its own types), so they cross as `Value`: a Rust mirror here would be a
/// second definition of a shape the daemon already owns, and the frontend
/// declares the same shapes in `shared/types/domain.ts`.
fn forward(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let client = connect()?;
    call(&client, method, params)
}

fn task_record(value: serde_json::Value) -> Result<TaskRecord, String> {
    serde_json::from_value(value).map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command(async)]
pub(crate) fn list_installed_plugins() -> Result<serde_json::Value, String> {
    forward("list_installed_plugins", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn list_container_resources(
    id: String,
    plugin: Option<String>,
) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    forward(
        "list_container_resources",
        serde_json::json!({ "id": id, "plugin": plugin }),
    )
}

#[tauri::command(async)]
pub(crate) fn list_resources() -> Result<serde_json::Value, String> {
    forward("list_resources", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn delete_resource(resource_id: String) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&resource_id) {
        return Err("invalid resource id".to_owned());
    }
    forward(
        "delete_resource",
        serde_json::json!({ "resourceId": resource_id }),
    )
}

#[tauri::command(async)]
pub(crate) fn list_resource_views() -> Result<serde_json::Value, String> {
    forward("list_resource_views", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn add_resource_view(
    kind: String,
    container: String,
    label: Option<String>,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&container) {
        return Err("invalid container id".to_owned());
    }
    forward(
        "add_resource_view",
        serde_json::json!({ "kind": kind, "container": container, "label": label, "path": path }),
    )
}

#[tauri::command(async)]
pub(crate) fn delete_resource_view(view_id: String) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&view_id) {
        return Err("invalid resource view id".to_owned());
    }
    forward("delete_resource_view", serde_json::json!({ "viewId": view_id }))
}

#[tauri::command(async)]
pub(crate) fn list_resource_type(
    kind: String,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    if kind.is_empty() {
        return Err("expected a resource kind".to_owned());
    }
    forward(
        "list_resource_type",
        serde_json::json!({ "kind": kind, "path": path }),
    )
}

#[tauri::command(async)]
pub(crate) fn browse_container_paths(
    id: String,
    path: Option<String>,
) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    forward(
        "browse_container_paths",
        serde_json::json!({ "id": id, "path": path }),
    )
}

#[tauri::command(async)]
pub(crate) fn read_resource_tree(
    id: String,
    path: String,
    section: Option<Vec<String>>,
) -> Result<serde_json::Value, String> {
    if !is_safe_identifier(&id) || path.is_empty() {
        return Err("invalid resource tree request".to_owned());
    }
    forward(
        "read_resource_tree",
        serde_json::json!({ "id": id, "path": path, "section": section }),
    )
}

#[tauri::command(async)]
pub(crate) fn enqueue_resource_extract(
    id: String,
    kind: String,
    entry: Option<String>,
    name: Option<String>,
    plugin: Option<String>,
    dest: Option<String>,
    out: Option<String>,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    // `dest` is a container-relative path; only `out` names a host file, and the
    // daemon's working directory is not the caller's.
    let value = forward(
        "enqueue_resource_extract",
        serde_json::json!({
            "id": id,
            "kind": kind,
            "entry": entry,
            "name": name,
            "plugin": plugin,
            "dest": dest,
            "out": out.as_deref().map(absolutize_path),
        }),
    )?;
    task_record(value)
}

#[tauri::command(async)]
pub(crate) fn enqueue_resource_inject(
    id: String,
    resource: Option<String>,
    from: Option<String>,
    input: Option<String>,
    kind: Option<String>,
    dest: Option<String>,
    entry: Option<String>,
    conflict: Option<String>,
    restart: Option<bool>,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    let value = forward(
        "enqueue_resource_inject",
        serde_json::json!({
            "id": id,
            "resource": resource,
            "from": from,
            "input": input.as_deref().map(absolutize_path),
            "kind": kind,
            "dest": dest,
            "entry": entry,
            "conflict": conflict,
            "restart": restart.unwrap_or(false),
        }),
    )?;
    task_record(value)
}

#[tauri::command(async)]
pub(crate) fn enqueue_resource_write(
    id: String,
    path: String,
    section: Option<Vec<String>>,
    text: String,
    conflict: Option<String>,
    restart: Option<bool>,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&id) || path.is_empty() {
        return Err("invalid resource write request".to_owned());
    }
    let value = forward(
        "enqueue_resource_write",
        serde_json::json!({
            "id": id,
            "path": path,
            "section": section.unwrap_or_default(),
            "text": text,
            "conflict": conflict,
            "restart": restart.unwrap_or(true),
        }),
    )?;
    task_record(value)
}
