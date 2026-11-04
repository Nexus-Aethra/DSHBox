//! RPC handlers for the node registry (roadmap stage A).
//!
//! Every handler here is synchronous: stage A only moves small JSON records.
//! The long file operations arrive in stage B via box-scheduler tasks.

use crate::state::DaemonState;
use serde_json::{json, Value};

/// Only the control plane (main token) may mint enroll grants. The grant is
/// returned to the caller, who injects it into the freshly supplied node.
pub(crate) fn node_enroll(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let kind = request["kind"].as_str().unwrap_or("").to_owned();
    let platform = request["platform"].as_str().unwrap_or("").to_owned();
    let capabilities: Vec<String> = request["capabilities"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let node_id = request["nodeId"].as_str().map(str::to_owned);
    let (enroll_token, id, epoch, expires_at) =
        state.nodes.enroll(&kind, &platform, capabilities, node_id)?;
    Ok(json!({
        "enrollToken": enroll_token,
        "nodeId": id,
        "controlUrl": state.nodes.control_url(),
        "epoch": epoch,
        "expiresAt": expires_at,
    }))
}

/// A helper's first act: exchange its one-shot enroll grant for a limited
/// session token. `enrollToken` travels in the body, not in the `token`
/// field, so the main-token gate in main.rs never sees it as a credential.
pub(crate) fn node_register(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let enroll_token = request["enrollToken"].as_str().unwrap_or("").to_owned();
    if enroll_token.is_empty() {
        return Err("expected enrollToken".to_owned());
    }
    let helper_version = request["helperVersion"].as_str().map(str::to_owned);
    let (node_id, session_token, epoch, heartbeat_seconds) =
        state.nodes.register(&enroll_token, helper_version)?;
    Ok(json!({
        "nodeId": node_id,
        "sessionToken": session_token,
        "epoch": epoch,
        "heartbeatSeconds": heartbeat_seconds,
    }))
}

pub(crate) fn node_heartbeat(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // nodeId comes from the validated session, never from the request body,
    // so a helper cannot heartbeat on behalf of another node.
    let node_id = request["__nodeId"]
        .as_str()
        .ok_or("session is not bound to a node")?;
    let epoch = request["epoch"].as_u64().unwrap_or(0);
    let status = state.nodes.heartbeat(node_id, epoch)?;
    Ok(json!({
        "ok": true,
        "epoch": epoch,
        "status": format!("{:?}", status).to_ascii_lowercase(),
    }))
}

pub(crate) fn node_list(state: &DaemonState, _request: &Value) -> Result<Value, String> {
    let nodes = state.nodes.list().map_err(|e| e.to_string())?;
    Ok(json!(nodes))
}

pub(crate) fn node_get(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let node = state
        .nodes
        .get(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node not found: {id}"))?;
    Ok(json!(node))
}

pub(crate) fn node_echo(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // Same rule as node_heartbeat: the node identity comes from the validated
    // session. Defaulting to "" instead would let a main-token call reach the
    // registry under an id that names no node.
    let node_id = request["__nodeId"]
        .as_str()
        .ok_or("session is not bound to a node")?;
    let text = request["text"].as_str().unwrap_or("").to_owned();
    state.nodes.report(node_id, "info", &format!("echo: {text}"));
    Ok(json!({ "text": text, "nodeId": node_id }))
}

pub(crate) fn node_report(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // A report is attributed to the node that made it, and only that node: an
    // empty or body-supplied id would let one guest write into another's log.
    let node_id = request["__nodeId"]
        .as_str()
        .ok_or("session is not bound to a node")?;
    let level = request["level"].as_str().unwrap_or("info").to_owned();
    let line = request["line"].as_str().unwrap_or("").to_owned();
    state.nodes.report(node_id, &level, &line);
    Ok(json!({ "ok": true }))
}

pub(crate) fn node_deregister(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // A session token may only deregister itself; the main token may remove
    // any node. The dispatch layer injects __nodeId for session callers.
    let id = request["__nodeId"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| request["id"].as_str().map(str::to_owned))
        .ok_or("expected id")?;
    state.nodes.deregister(&id)?;
    Ok(json!({ "ok": true, "id": id }))
}
