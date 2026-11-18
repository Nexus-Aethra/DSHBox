use super::super::*;

/// The node registry, as the daemon holds it.
///
/// A thin forwarder, like the container one: the daemon owns the registry
/// and this only carries the reply across the process boundary. There is no
/// filtering or shaping here, because a second opinion about what a node is
/// would be a second place for the two to disagree.
#[tauri::command(async)]
pub(crate) fn list_nodes() -> Result<Vec<serde_json::Value>, String> {
    let client = connect()?;
    let value = call(&client, "node_list", serde_json::json!({}))?;
    serde_json::from_value(value).map_err(|error| format!("invalid node list: {error}"))
}

/// One node by id.
#[tauri::command(async)]
pub(crate) fn get_node(id: String) -> Result<serde_json::Value, String> {
    let client = connect()?;
    let value = call(&client, "node_get", serde_json::json!({ "id": id }))?;
    Ok(value)
}

/// Remove a node from the registry.
///
/// The control plane only: a node cannot deregister another node, and the
/// daemon enforces that by refusing a session token here.
#[tauri::command(async)]
pub(crate) fn deregister_node(id: String) -> Result<serde_json::Value, String> {
    let client = connect()?;
    call(&client, "node_deregister", serde_json::json!({ "id": id }))
}

/// Mint an enroll grant and the bootstrap secret that goes with it.
///
/// The reply carries the seed once. The UI shows it in a copyable field and
/// never logs it, which is the same discipline the daemon applies to the RPC
/// reply itself.
#[tauri::command(async)]
pub(crate) fn enroll_node(
    kind: String,
    platform: String,
    node_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let client = connect()?;
    let mut request = serde_json::json!({ "kind": kind, "platform": platform });
    if let Some(id) = node_id {
        if !id.trim().is_empty() {
            request["nodeId"] = serde_json::json!(id);
        }
    }
    call(&client, "node_enroll", request)
}
