//! Daemon event bus for SSE streaming.
//!
//! Every task progress update, log line, and resource change is broadcast
//! to all connected `/events` subscribers. The daemon writes the event
//! stream as `text/event-stream` over a persistent HTTP connection; clients
//! (CLI `--watch`, desktop event proxy, or `curl -N`) consume it as a
//! standard SSE feed.

use std::{collections::BTreeMap, sync::Mutex};

/// A single event the daemon broadcasts to all `/events` subscribers.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
#[allow(
    dead_code,
    reason = "reserved event vocabulary not yet emitted by the daemon"
)]
pub enum DaemonEvent {
    /// A task changed stage or progress.
    TaskStage {
        task_id: String,
        stage: String,
        progress: u8,
    },
    /// A task produced a log line.
    TaskLog { task_id: String, line: String },
    /// A task finished (success or failure).
    TaskFinished {
        task_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Rollback was initiated for a failed task.
    RollbackStarted { task_id: String },
    /// Rollback completed successfully for a failed task.
    RollbackFinished { task_id: String },
    /// Rollback itself failed (the task ends in Failed with rollback_error).
    RollbackFailed { task_id: String, error: String },
    /// A resource was added to the resource map.
    ResourceAdded {
        id: String,
        r#type: String,
        name: String,
    },
    /// A resource was removed (soft-deleted) from the resource map.
    ResourceRemoved { id: String },
    /// A resource's status changed (e.g., active → deleted).
    ResourceUpdated { id: String, status: String },
    /// Remote tags were fetched (result of `fetch_remote_tags`).
    TagsFetched { tags: Vec<String> },
    /// A background update check finished; clients re-read `get_update_state`.
    ///
    /// An invalidation notice rather than the payload: the reader already
    /// exists, and pushing the whole state here would be a second shape of
    /// the same data to keep in step.
    UpdateState,
    /// A task's declared stages changed (started, progressed or finished).
    ///
    /// Separate from `TaskStage`, which carries only the one-line summary the
    /// old single bar needs. The per-stage picture has its own shape, and
    /// folding it into that event would make both harder to read.
    TaskStages {
        task_id: String,
        stages: serde_json::Value,
    },
    /// A box-helper completed its registration handshake.
    NodeRegistered {
        id: String,
        kind: String,
        platform: String,
    },
    /// A node changed status (online / offline / removed).
    NodeStatus { id: String, status: String },
    /// A helper reported a log line to the control plane.
    NodeLog {
        id: String,
        level: String,
        line: String,
    },
}

impl DaemonEvent {
    /// SSE event name (the `event:` field).
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::TaskStage { .. } => "task_stage",
            Self::TaskLog { .. } => "task_log",
            Self::TaskFinished { .. } => "task_finished",
            Self::RollbackStarted { .. } => "rollback_started",
            Self::RollbackFinished { .. } => "rollback_finished",
            Self::RollbackFailed { .. } => "rollback_failed",
            Self::ResourceAdded { .. } => "resource_added",
            Self::ResourceRemoved { .. } => "resource_removed",
            Self::ResourceUpdated { .. } => "resource_updated",
            Self::TagsFetched { .. } => "tags_fetched",
            Self::UpdateState => "update_state",
            Self::TaskStages { .. } => "task_stages",
            Self::NodeRegistered { .. } => "node_registered",
            Self::NodeStatus { .. } => "node_status",
            Self::NodeLog { .. } => "node_log",
        }
    }

    /// Whether this event belongs to the node surface. A helper session
    /// token may subscribe to the event stream, but only to what its
    /// whitelist grants it; task and resource events are control-plane
    /// state, not node state.
    ///
    /// Written as an exhaustive match rather than a wildcard so a new
    /// variant forces a decision here instead of defaulting to "not a node
    /// event" and vanishing from the surface without anyone noticing.
    pub fn is_node_event(&self) -> bool {
        match self {
            Self::NodeRegistered { .. } | Self::NodeStatus { .. } | Self::NodeLog { .. } => true,
            Self::TaskStage { .. }
            | Self::TaskStages { .. }
            | Self::TaskLog { .. }
            | Self::TaskFinished { .. }
            | Self::RollbackStarted { .. }
            | Self::RollbackFinished { .. }
            | Self::RollbackFailed { .. }
            | Self::ResourceAdded { .. }
            | Self::ResourceRemoved { .. }
            | Self::ResourceUpdated { .. }
            | Self::TagsFetched { .. }
            | Self::UpdateState => false,
        }
    }

    /// The node this event belongs to, so a subscriber can be shown its
    /// own node events only.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::NodeRegistered { id, .. }
            | Self::NodeStatus { id, .. }
            | Self::NodeLog { id, .. } => Some(id),
            _ => None,
        }
    }
}

type SubscriberId = u64;

/// Global event bus. All daemon subsystems push events here; the SSE
/// handler reads them and forwards to connected clients.
pub struct DaemonEvents {
    subs: Mutex<BTreeMap<SubscriberId, std::sync::mpsc::Sender<DaemonEvent>>>,
    next_id: Mutex<SubscriberId>,
}

impl DaemonEvents {
    pub fn new() -> Self {
        Self {
            subs: Mutex::new(BTreeMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Subscribe to the event stream. Returns a `(SubscriberId, Receiver)`
    /// pair the caller uses to poll for events.
    pub fn subscribe(&self) -> (SubscriberId, std::sync::mpsc::Receiver<DaemonEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut id = self.next_id.lock().unwrap();
        let sid = *id;
        *id += 1;
        self.subs.lock().unwrap().insert(sid, tx);
        (sid, rx)
    }

    /// Remove a subscriber (e.g., on client disconnect).
    /// Kept for future explicit-disconnect support; SSE connections are
    /// currently cleaned up implicitly by `broadcast` pruning dropped channels.
    #[allow(dead_code)]
    pub fn unsubscribe(&self, id: SubscriberId) {
        self.subs.lock().unwrap().remove(&id);
    }

    /// Broadcast an event to all current subscribers. Dropped receivers
    /// (disconnected clients) are silently pruned.
    pub fn broadcast(&self, event: DaemonEvent) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|_id, tx| tx.send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every node event, spelled out. A helper session token can subscribe
    /// to the event stream, so this list is the whole surface reachable from
    /// inside a guest: adding to it is a decision, not an accident.
    fn node_events() -> Vec<DaemonEvent> {
        vec![
            DaemonEvent::NodeRegistered {
                id: String::from("n1"),
                kind: String::from("wsl2"),
                platform: String::from("linux-x64"),
            },
            DaemonEvent::NodeStatus { id: String::from("n1"), status: String::from("online") },
            DaemonEvent::NodeLog {
                id: String::from("n1"),
                level: String::from("info"),
                line: String::from("hi"),
            },
        ]
    }

    #[test]
    fn the_node_surface_is_exactly_the_node_events() {
        for event in node_events() {
            assert!(event.is_node_event(), "{} must be a node event", event.event_name());
            assert_eq!(event.node_id(), Some("n1"));
        }
        let control = [
            DaemonEvent::TaskStage { task_id: String::from("t"), stage: String::from("s"), progress: 1 },
            DaemonEvent::TaskLog { task_id: String::from("t"), line: String::from("l") },
            DaemonEvent::TaskFinished { task_id: String::from("t"), status: String::from("done"), error: None },
            DaemonEvent::RollbackStarted { task_id: String::from("t") },
            DaemonEvent::RollbackFinished { task_id: String::from("t") },
            DaemonEvent::RollbackFailed { task_id: String::from("t"), error: String::from("e") },
            DaemonEvent::ResourceAdded { id: String::from("r"), r#type: String::from("credentials"), name: String::from("c") },
            DaemonEvent::ResourceRemoved { id: String::from("r") },
            DaemonEvent::ResourceUpdated { id: String::from("r"), status: String::from("deleted") },
            DaemonEvent::TagsFetched { tags: vec![] },
            DaemonEvent::UpdateState,
        ];
        for event in control {
            assert!(!event.is_node_event(), "{} must not reach a session subscriber", event.event_name());
            assert_eq!(event.node_id(), None);
        }
    }

    #[test]
    fn node_event_names_are_the_wire_names() {
        // The UI switches on these strings and the helper prints them; a
        // rename is a silent drop on both sides, so the wire spelling is
        // pinned here rather than remembered.
        let names: Vec<&str> = node_events().iter().map(|e| e.event_name()).collect();
        assert_eq!(names, vec!["node_registered", "node_status", "node_log"]);
    }
}
