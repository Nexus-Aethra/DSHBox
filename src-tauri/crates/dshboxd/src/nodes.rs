//! Node registry for the control plane (roadmap stage A).
//!
//! A node is one real execution environment: the daemon's own host process,
//! a WSL2 distro, a libkrun microVM, an Apple VZ VM, a docker container…
//! Stage A only owns the *registry and the registration handshake*; it does
//! not start containers or move files. See docs/design/2026-11-04-node-core.md.

use box_foundation::{now_seconds, BoxResult, BoxPaths};
use box_foundation::collection::Collection;
use box_store::open_document_store_for_paths;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
};

/// Wire status of a node. Lowercase on the wire, like box-scheduler's task
/// states; a test pins the spelling so a rename cannot silently drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Registering,
    Online,
    Offline,
    Draining,
    Failed,
}

/// One execution environment the control plane knows about.
///
/// Deliberately does NOT carry DSH versions, images or plugins: those are
/// NodeArtifact (stage B) and profile state (stage C). A node is a machine,
/// not a workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRecord {
    pub id: String,
    /// "host" | "wsl2" | "libkrun" | "vz" | "docker"
    pub kind: String,
    /// e.g. "windows-x64" / "linux-x64"
    pub platform: String,
    pub status: NodeStatus,
    pub capabilities: Vec<String>,
    /// Address the helper calls back on; None for the in-process host node.
    pub control_url: Option<String>,
    /// Bumped every daemon start. A heartbeat carrying an old epoch is
    /// rejected, so tokens are never silently reusable across restarts.
    pub session_epoch: u64,
    pub enrolled_at: Option<u64>,
    pub last_seen: Option<u64>,
    pub helper_version: Option<String>,
    /// Backend-specific reference (distro name, container id, …). Stage A
    /// never reads it; the supply adapter in stage C will.
    pub backend_ref: Option<String>,
    /// The daemon's own host. Built-in, never heartbeats, never expires,
    /// cannot be deregistered.
    pub host_node: bool,
}

fn node_key(record: &NodeRecord) -> &str {
    record.id.as_str()
}

pub const HOST_NODE_ID: &str = "host";
pub const HEARTBEAT_SECONDS: u64 = 30;
/// Three missed heartbeats before an online node is marked offline.
pub const OFFLINE_AFTER_SECONDS: u64 = HEARTBEAT_SECONDS * 3;
pub const ENROLL_TTL_SECONDS: u64 = 300;

/// Methods a session token may call. The global discovery token keeps every
/// other RPC; a helper that leaks its session token cannot drive the box.
pub const NODE_METHODS: &[&str] = &[
    "node_heartbeat",
    "node_report",
    "node_echo",
    "node_deregister",
];

/// Constant-time comparison of two bearer strings. Avoids leaking token
/// bytes through early-exit timing.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..max {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

struct EnrollGrant {
    node_id: String,
    kind: String,
    platform: String,
    capabilities: Vec<String>,
    expires_at: u64,
}

struct NodeSession {
    token: String,
    epoch: u64,
}

/// Shared node state. Clones share the same registry, sessions and grants.
#[derive(Clone)]
pub struct NodeManager {
    collection: Collection<NodeRecord>,
    /// In-memory only: daemon restart invalidates every session.
    sessions: Arc<Mutex<BTreeMap<String, NodeSession>>>,
    grants: Arc<Mutex<BTreeMap<String, EnrollGrant>>>,
    epoch: Arc<RwLock<u64>>,
    /// Set by the daemon on startup; the helper learns it from node_enroll.
    control_url: Arc<RwLock<String>>,
    events: Arc<crate::events::DaemonEvents>,
}

impl NodeManager {
    /// Build the manager. Uses the SQLite document store when a runtime is
    /// configured, and an in-memory collection otherwise (fresh install or
    /// unit tests) — the same degradation the task queue uses.
    pub fn load(paths: &BoxPaths, control_url: String, events: Arc<crate::events::DaemonEvents>) -> Self {
        let collection: Collection<NodeRecord> = match open_document_store_for_paths(paths) {
            Ok(store) => Collection::new(store, "nodes", node_key),
            Err(error) => {
                tracing::warn!("node registry using in-memory store: {error}");
                Collection::memory("nodes", node_key)
            }
        };
        let epoch = now_seconds();
        let manager = Self {
            collection,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            grants: Arc::new(Mutex::new(BTreeMap::new())),
            epoch: Arc::new(RwLock::new(epoch)),
            control_url: Arc::new(RwLock::new(control_url)),
            events,
        };
        // Idempotent: the host node always exists and is always online.
        let _ = manager.upsert_host_node();
        // Sessions live in memory only, so a restart kills every one of them:
        // a persisted `online` from the previous process is already false by
        // the time we load it. Demote at load instead of waiting up to one
        // sweep interval for the registry to tell the truth.
        manager.demote_orphans_after_restart();
        manager
    }

    /// No session can survive a daemon restart, so any non-host node recorded
    /// as `online` lost its session with the previous process. Mark it offline
    /// now; the record (and its artifacts) stay, so a re-enrolled helper goes
    /// straight back to online.
    ///
    /// Nothing connected to the previous daemon can still be attached to this
    /// one, so what this says is true rather than a guess.
    pub fn demote_orphans_after_restart(&self) -> usize {
        let mut changed = 0;
        for mut node in self.list().unwrap_or_default() {
            if node.host_node || node.status != NodeStatus::Online {
                continue;
            }
            node.status = NodeStatus::Offline;
            if self.collection.upsert(&[node]).is_ok() {
                changed += 1;
            }
        }
        changed
    }

    fn upsert_host_node(&self) -> BoxResult<()> {
        let existing = self.collection.load_all().unwrap_or_default();
        if let Some(mut host) = existing.into_iter().find(|n| n.id == HOST_NODE_ID) {
            host.status = NodeStatus::Online;
            host.host_node = true;
            return self.collection.upsert(&[host]);
        }
        self.collection.upsert(&[NodeRecord {
            id: HOST_NODE_ID.to_owned(),
            kind: "host".to_owned(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            status: NodeStatus::Online,
            capabilities: vec!["exec".to_owned(), "build".to_owned(), "copy".to_owned(), "dsh-host".to_owned()],
            control_url: None,
            session_epoch: self.current_epoch(),
            enrolled_at: Some(now_seconds()),
            last_seen: Some(now_seconds()),
            helper_version: None,
            backend_ref: None,
            host_node: true,
        }])
    }

    pub fn current_epoch(&self) -> u64 {
        *self.epoch.read().unwrap_or_else(|p| p.into_inner())
    }

    pub fn control_url(&self) -> String {
        self.control_url.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The TCP port is assigned by the OS at bind time, after DaemonState is
    /// built. main.rs calls this once the listener is live; enroll answers
    /// must carry the reachable URL, not a placeholder.
    pub fn set_control_url(&self, url: String) {
        *self.control_url.write().unwrap_or_else(|p| p.into_inner()) = url;
    }

    pub fn list(&self) -> BoxResult<Vec<NodeRecord>> {
        let mut nodes = self.collection.load_all()?;
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(nodes)
    }

    pub fn get(&self, id: &str) -> BoxResult<Option<NodeRecord>> {
        Ok(self.collection.load_all()?.into_iter().find(|n| n.id == id))
    }

    fn set_status(&self, id: &str, status: NodeStatus) -> BoxResult<()> {
        if let Some(mut node) = self.get(id)? {
            node.status = status;
            if status == NodeStatus::Online {
                node.last_seen = Some(now_seconds());
            }
            self.collection.upsert(&[node.clone()])?;
            self.events.broadcast(crate::events::DaemonEvent::NodeStatus {
                id: id.to_owned(),
                status: format!("{:?}", status).to_ascii_lowercase(),
            });
            let _ = node;
        }
        Ok(())
    }

    /// Issue a one-shot enroll grant. Only the control plane (main token)
    /// calls this; the grant is never persisted and expires quickly.
    pub fn enroll(
        &self,
        kind: &str,
        platform: &str,
        capabilities: Vec<String>,
        node_id: Option<String>,
    ) -> Result<(String, String, u64, u64), String> {
        if kind.trim().is_empty() || !box_foundation::is_safe_identifier(kind) {
            return Err("invalid node kind".to_owned());
        }
        let id = node_id
            .filter(|v| !v.is_empty() && box_foundation::is_safe_identifier(v))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = now_seconds() + ENROLL_TTL_SECONDS;
        self.grants.lock().unwrap().insert(
            token.clone(),
            EnrollGrant {
                node_id: id.clone(),
                kind: kind.to_owned(),
                platform: platform.to_owned(),
                capabilities,
                expires_at,
            },
        );
        Ok((token, id, self.current_epoch(), expires_at))
    }

    /// Consume an enroll grant, create/refresh the NodeRecord, issue a
    /// limited session token. A grant can only be used once.
    pub fn register(
        &self,
        enroll_token: &str,
        helper_version: Option<String>,
    ) -> Result<(String, String, u64, u64), String> {
        let grant = {
            let mut grants = self.grants.lock().unwrap();
            let grant = grants
                .remove(enroll_token)
                .ok_or("enroll token is unknown or already used")?;
            if grant.expires_at < now_seconds() {
                return Err("enroll token expired".to_owned());
            }
            grant
        };
        let epoch = self.current_epoch();
        let session = uuid::Uuid::new_v4().to_string();
        let now = now_seconds();
        let record = NodeRecord {
            id: grant.node_id.clone(),
            kind: grant.kind,
            platform: grant.platform,
            status: NodeStatus::Online,
            capabilities: grant.capabilities,
            control_url: Some(self.control_url()),
            session_epoch: epoch,
            enrolled_at: Some(now),
            last_seen: Some(now),
            helper_version,
            backend_ref: None,
            host_node: false,
        };
        self.collection
            .upsert(&[record.clone()])
            .map_err(|e| e.to_string())?;
        self.sessions.lock().unwrap().insert(
            grant.node_id.clone(),
            NodeSession {
                token: session.clone(),
                epoch,
            },
        );
        self.events.broadcast(crate::events::DaemonEvent::NodeRegistered {
            id: record.id.clone(),
            kind: record.kind.clone(),
            platform: record.platform.clone(),
        });
        Ok((grant.node_id, session, epoch, HEARTBEAT_SECONDS))
    }

    /// True when a bearer is a live session token for `node_id`, with the
    /// current epoch. Used by both /rpc and /events.
    pub fn validate_session(&self, bearer: Option<&str>, node_id: Option<&str>, epoch: u64) -> bool {
        let Some(bearer) = bearer else { return false };
        if epoch != self.current_epoch() {
            return false;
        }
        let sessions = self.sessions.lock().unwrap();
        match node_id {
            Some(id) => sessions
                .get(id)
                .is_some_and(|s| s.epoch == epoch && constant_time_eq(&s.token, bearer)),
            None => sessions
                .values()
                .any(|s| s.epoch == epoch && constant_time_eq(&s.token, bearer)),
        }
    }

    pub fn is_node_method(method: &str) -> bool {
        NODE_METHODS.contains(&method)
    }

    /// Resolve a session bearer to the node it belongs to (current epoch
    /// required). Used by main.rs to bind the request to `__nodeId`.
    pub fn session_node_for(&self, bearer: Option<&str>, epoch: u64) -> Option<String> {
        let bearer = bearer?;
        if epoch != self.current_epoch() {
            return None;
        }
        let sessions = self.sessions.lock().unwrap();
        sessions
            .iter()
            .find(|(_, s)| s.epoch == epoch && constant_time_eq(&s.token, bearer))
            .map(|(id, _)| id.clone())
    }

    pub fn heartbeat(&self, node_id: &str, epoch: u64) -> Result<NodeStatus, String> {
        if epoch != self.current_epoch() {
            return Err("stale epoch; re-enroll and register again".to_owned());
        }
        let mut record = self
            .get(node_id)
            .ok()
            .flatten()
            .ok_or("node is not registered")?;
        if record.host_node {
            return Err("the host node does not heartbeat".to_owned());
        }
        record.status = NodeStatus::Online;
        record.last_seen = Some(now_seconds());
        record.session_epoch = epoch;
        self.collection
            .upsert(&[record])
            .map_err(|e| e.to_string())?;
        Ok(NodeStatus::Online)
    }

    pub fn report(&self, node_id: &str, level: &str, line: &str) {
        tracing::info!(node = node_id, level = level, "helper: {line}");
        self.events.broadcast(crate::events::DaemonEvent::NodeLog {
            id: node_id.to_owned(),
            level: level.to_owned(),
            line: line.to_owned(),
        });
    }

    pub fn deregister(&self, id: &str) -> Result<(), String> {
        let record = self
            .get(id)
            .ok()
            .flatten()
            .ok_or("node is not registered")?;
        if record.host_node {
            return Err("the built-in host node cannot be deregistered".to_owned());
        }
        self.collection.remove(id).map_err(|e| e.to_string())?;
        self.sessions.lock().unwrap().remove(id);
        self.events.broadcast(crate::events::DaemonEvent::NodeStatus {
            id: id.to_owned(),
            status: "removed".to_owned(),
        });
        Ok(())
    }

    /// Mark online nodes whose last heartbeat is older than the threshold as
    /// offline. Records are kept (their artifacts/backend refs remain), so a
    /// returning helper can go straight back to online.
    pub fn sweep(&self) -> usize {
        let now = now_seconds();
        let mut changed = 0;
        for node in self.list().unwrap_or_default() {
            if node.host_node || node.status != NodeStatus::Online {
                continue;
            }
            let last = node.last_seen.unwrap_or(0);
            if now.saturating_sub(last) > OFFLINE_AFTER_SECONDS {
                if self.set_status(&node.id, NodeStatus::Offline).is_ok() {
                    changed += 1;
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::DaemonEvents;

    fn manager() -> NodeManager {
        NodeManager::load(
            &BoxPaths { config: std::path::PathBuf::from("/nonexistent"), runtime: None },
            "http://127.0.0.1:1".to_owned(),
            Arc::new(DaemonEvents::new()),
        )
    }

    #[test]
    fn node_status_is_lowercase_on_the_wire() {
        assert_eq!(serde_json::to_string(&NodeStatus::Online).unwrap(), "\"online\"");
        assert_eq!(serde_json::to_string(&NodeStatus::Registering).unwrap(), "\"registering\"");
    }

    #[test]
    fn host_node_is_built_in_online_and_protected() {
        let m = manager();
        let host = m.get(HOST_NODE_ID).unwrap().unwrap();
        assert!(host.host_node);
        assert_eq!(host.status, NodeStatus::Online);
        assert!(m.deregister(HOST_NODE_ID).is_err());
        assert!(m.heartbeat(HOST_NODE_ID, m.current_epoch()).is_err());
    }

    #[test]
    fn enroll_register_and_scoped_session() {
        let m = manager();
        let (grant, id, epoch, _) = m.enroll("wsl2", "linux-x64", vec!["exec".to_owned()], None).unwrap();
        assert!(m.enroll("wsl2", "linux-x64", vec![], None).is_ok());
        let (session, hb) = {
            let (rid, token, ep, hb) = m.register(&grant, Some("0.1.0".to_owned())).unwrap();
            assert_eq!(rid, id);
            assert_eq!(ep, epoch);
            (token, hb)
        };
        assert_eq!(hb, HEARTBEAT_SECONDS);
        // A second use of the same one-shot grant must fail.
        assert!(m.register(&grant, None).is_err());
        // The session is valid for its own node and current epoch…
        assert!(m.validate_session(Some(&session), Some(&id), epoch));
        // …but not for another node, a stale epoch, or as a global token.
        assert!(!m.validate_session(Some(&session), Some("other"), epoch));
        assert!(!m.validate_session(Some(&session), Some(&id), epoch + 1));
        assert!(!m.validate_session(None, Some(&id), epoch));
        // Whitelist: a session token cannot reach control-plane methods.
        assert!(NodeManager::is_node_method("node_heartbeat"));
        assert!(!NodeManager::is_node_method("list_containers"));
        assert!(!NodeManager::is_node_method("delete_container"));
        let node = m.get(&id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Online);
        assert_eq!(node.kind, "wsl2");
        assert_eq!(node.platform, "linux-x64");
    }

    #[test]
    fn heartbeat_updates_last_seen_and_stale_epoch_is_rejected() {
        let m = manager();
        let (grant, id, epoch, _) = m.enroll("libkrun", "linux-x64", vec![], None).unwrap();
        let (session, _, _, _) = m.register(&grant, None).unwrap();
        let _ = session;
        assert_eq!(m.heartbeat(&id, epoch).unwrap(), NodeStatus::Online);
        assert!(m.heartbeat(&id, epoch + 1).is_err());
    }

    #[test]
    fn sweep_marks_only_stale_non_host_nodes_offline() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("docker", "linux-x64", vec![], None).unwrap();
        m.register(&grant, None).unwrap();
        // Force the record into the past without sleeping.
        let mut node = m.get(&id).unwrap().unwrap();
        node.last_seen = Some(now_seconds() - OFFLINE_AFTER_SECONDS - 1);
        m.collection.upsert(&[node]).unwrap();
        assert_eq!(m.sweep(), 1);
        assert_eq!(m.get(&id).unwrap().unwrap().status, NodeStatus::Offline);
        // The host node never expires.
        assert_eq!(m.get(HOST_NODE_ID).unwrap().unwrap().status, NodeStatus::Online);
        assert_eq!(m.sweep(), 0);
    }

    #[test]
    fn restart_demotes_surviving_online_nodes_but_keeps_records() {
        // Sessions are memory-only, so a persisted `online` is a lie the moment
        // the previous process is gone. Loading a fresh manager must correct it
        // at once rather than after a sweep interval.
        // A real runtime dir, so both managers open the same document store
        // and the second one genuinely reads what the first wrote.
        let root = std::env::temp_dir().join(format!("dshboxd-nodes-restart-{}", now_seconds()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = BoxPaths { config: root.join("config.json"), runtime: Some(root.clone()) };
        let first = NodeManager::load(&paths, "http://127.0.0.1:1".to_owned(), Arc::new(DaemonEvents::new()));
        let (grant, id, _, _) = first.enroll("wsl2", "linux-x64", vec![], Some("survivor".to_owned())).unwrap();
        first.register(&grant, None).unwrap();
        assert_eq!(first.get(&id).unwrap().unwrap().status, NodeStatus::Online);

        let reloaded = NodeManager::load(&paths, "http://127.0.0.1:2".to_owned(), Arc::new(DaemonEvents::new()));
        let record = reloaded.get(&id).unwrap().expect("record survives the restart");
        assert_eq!(record.status, NodeStatus::Offline);
        assert!(record.enrolled_at.is_some(), "the record is kept, not deleted");
        // The host node is ours, not a survivor, and stays online.
        assert_eq!(reloaded.get(HOST_NODE_ID).unwrap().unwrap().status, NodeStatus::Online);
        // Demoting is idempotent.
        assert_eq!(reloaded.demote_orphans_after_restart(), 0);
    }

    #[test]
    fn constant_time_eq_handles_lengths_and_empty() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn node_record_round_trips() {
        let record = NodeRecord {
            id: "n1".to_owned(),
            kind: "wsl2".to_owned(),
            platform: "linux-x64".to_owned(),
            status: NodeStatus::Draining,
            capabilities: vec!["exec".to_owned()],
            control_url: Some("http://127.0.0.1:1".to_owned()),
            session_epoch: 7,
            enrolled_at: Some(1),
            last_seen: Some(2),
            helper_version: Some("0.1.0".to_owned()),
            backend_ref: None,
            host_node: false,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: NodeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert!(json.contains("\"hostNode\":false"));
        assert!(json.contains("\"sessionEpoch\":7"));
    }
}
