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
    // The helper reports the platform it compiled for; the daemon validates and
    // stores that, not the string the supply adapter typed at enroll (§2.7).
    let helper_platform = request["platform"].as_str().map(str::to_owned);
    let (node_id, session_token, epoch, heartbeat_seconds) =
        state
            .nodes
            .register(&enroll_token, helper_version, helper_platform)?;
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

/// Control-plane read over the artifact index (stage B1).
///
/// Synchronous on purpose: it is a small local read, and the design puts
/// control-plane reads on the synchronous side of the split (section 3.5).
pub(crate) fn node_artifact_list(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let node_id = request["nodeId"].as_str().filter(|value| !value.is_empty());
    let base_id = request["baseId"].as_str().filter(|value| !value.is_empty());
    let rows = state
        .artifacts
        .list(node_id, base_id)
        .map_err(|error| error.to_string())?;
    Ok(json!(rows))
}

/// A node reports a base it has just built and hashed.
///
/// The node identity comes from the validated session, never from the body, so a
/// helper cannot report an artifact on another node's behalf (invariant I3). The
/// row's platform is then checked against that same node's platform, which is
/// the one cross-record fact the control plane is in a position to verify (I1).
pub(crate) fn node_artifact_report(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let node_id = request["__nodeId"]
        .as_str()
        .ok_or("session is not bound to a node")?
        .to_owned();
    let node = state
        .nodes
        .get(&node_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("node not found: {node_id}"))?;
    let report: crate::node_artifacts::ArtifactReport = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid artifact report: {error}"))?;
    let record = state.artifacts.report(&node, &report)?;
    Ok(json!(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DaemonState;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use box_state::ResourceStateManager;
    use std::sync::{Arc, Mutex, RwLock};

    fn state() -> DaemonState {
        let events = Arc::new(crate::events::DaemonEvents::new());
        let empty = BoxPaths {
            config: std::path::PathBuf::from("/nonexistent"),
            runtime: None,
        };
        DaemonState {
            manager: TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: std::path::PathBuf::from("/nonexistent"),
                runtime: None,
            }),
            containers: Arc::new(Default::default()),
            resources: ResourceStateManager::default(),
            events: events.clone(),
            browser: Mutex::new(crate::debug::Sessions::new()),
            nodes: Arc::new(crate::nodes::NodeManager::load(
                &empty,
                String::new(),
                events,
            )),
            artifacts: crate::node_artifacts::ArtifactIndex::memory(),
        }
    }

    fn report_body(base_id: &str, platform: &str) -> Value {
        json!({
            "baseId": base_id,
            "sourceCommit": "0d1f500",
            "platform": platform,
            "archive": "/var/dshbox/artifacts/base.tar",
            "sha256": format!("9f2c{}", "a".repeat(60)),
            "sizeBytes": 1024,
        })
    }

    /// Invariant I3 at the RPC boundary: the node a report belongs to is the
    /// session's node, never a `nodeId` the body supplies. A helper that
    /// could name another node would let one guest poison another's index.
    #[test]
    fn a_report_is_attributed_to_the_session_node_not_the_body() {
        let state = state();
        let (grant, id, _, _) = state
            .nodes
            .enroll("wsl2", "linux-x86_64", vec!["build".to_owned()], None)
            .unwrap();
        state.nodes.register(&grant, None, None).unwrap();

        let mut request = report_body("base-9c1f00d4a1b2c3d4", "linux-x86_64");
        request["__nodeId"] = json!(id);
        request["nodeId"] = json!("node-attacker"); // forged; must be ignored
        let row = node_artifact_report(&state, &request).unwrap();

        assert_eq!(row["nodeId"], id, "the session wins, not the body");
        assert_eq!(row["id"], format!("{id}|base-9c1f00d4a1b2c3d4"));
        let rows = node_artifact_list(&state, &json!({ "nodeId": id })).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["nodeId"], id);
    }

    /// I1 through the same handler: a report whose platform is the host's,
    /// not the (Linux) node's, is refused and writes nothing the control
    /// plane could later mistake for a usable base.
    #[test]
    fn a_report_for_another_platform_is_refused_through_the_rpc() {
        let state = state();
        let (grant, id, _, _) = state
            .nodes
            .enroll("wsl2", "linux-x86_64", vec!["build".to_owned()], None)
            .unwrap();
        state.nodes.register(&grant, None, None).unwrap();

        let mut request = report_body("base-9c1f00d4a1b2c3d4", "windows-x86_64");
        request["__nodeId"] = json!(id);
        assert!(node_artifact_report(&state, &request).is_err());
        let rows = node_artifact_list(&state, &json!({})).unwrap();
        assert!(rows.as_array().unwrap().is_empty());
    }
}
