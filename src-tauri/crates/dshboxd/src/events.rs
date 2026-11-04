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
    /// A resource's status changed (e.g., active 鈫?deleted).
    ResourceUpdated { id: String, status: String },
    /// Remote tags were fetched (result of `fetch_remote_tags`).
    TagsFetched { tags: Vec<String> },
    /// A background update check finished; clients re-read `get_update_state`.
    ///
    /// An invalidation notice rather than the payload: the reader already
    /// exists, and pushing the whole state here would be a second shape of
    /// the same data to keep in step.
    UpdateState,
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
            Self::NodeRegistered { .. } => "node_registered",
            Self::NodeStatus { .. } => "node_status",
            Self::NodeLog { .. } => "node_log",
        }
    }

    /// Whether this event belongs to the node surface. A helper's session
    /// token may subscribe to `/events`, but only to what its whitelist grants
    /// it; task and resource events are control-plane state, not node state.
    ///
    /// Kept next to `event_name` on purpose: a new node event that forgets this
    /// match is a new event a helper silently never receives, and the two lists
    /// being adjacent is what makes that omission visible.
    pub fn is_node_event(&self) -> bool {
        matches!(
            self,
            Self::NodeRegistered { .. } | Self::NodeStatus { .. } | Self::NodeLog { .. }
        )
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

    /// The set of node events, spelled out once. A helper's session token may
    /// subscribe to `/events`, so anything in this list is reachable from
    /// inside a guest: adding to it is a decision, not an accident.
    fn node_events() -> Vec<DaemonEvent> {
        vec![
            DaemonEvent::NodeRegistered {
                id: "n".to_owned(),
                kind: "wsl2".to_owned(),
                platform: "linux-x64".to_owned(),
            },
            DaemonEvent::NodeStatus {
                id: "n".to_owned(),
                status: "online".to_owned(),
            },
            DaemonEvent::NodeLog {
                id: "n".to_owned(),
                level: "info".to_owned(),
                line: "hi".to_owned(),
            },
        ]
    }

    #[test]
    fn node_events_are_exactly_the_node_surface() {
        for event in node_events() {
            assert!(event.is_node_event(), "{} must be a node event", event.event_name());
        }
        // Control-plane events are not node events, whatever their name looks
        // like. This is what stops a task log from reaching a guest.
        let control = [
            DaemonEvent::TaskStage { task_id: "t".to_owned(), stage: "preparing".to_owned(), progress: 1 },
            DaemonEvent::TaskLog { task_id: "t".to_owned(), line: "l".to_owned() },
            DaemonEvent::TaskFinished { task_id: "t".to_owned(), status: "done".to_owned(), error: None },
            DaemonEvent::ResourceAdded { id: "r".to_owned(), r#type: "credentials".to_owned(), name: "c".to_owned() },
            DaemonEvent::ResourceRemoved { id: "r".to_owned() },
            DaemonEvent::ResourceUpdated { id: "r".to_owned(), status: "deleted".to_owned() },
            DaemonEvent::TagsFetched { tags: vec![] },
            DaemonEvent::UpdateState,
            DaemonEvent::RollbackStarted { task_id: "t".to_owned() },
            DaemonEvent::RollbackFinished { task_id: "t".to_owned() },
            DaemonEvent::RollbackFailed { task_id: "t".to_owned(), error: "e".to_owned() },
        ];
        for event in control {
            assert!(!event.is_node_event(), "{} must not reach a session subscriber", event.event_name());
        }
    }

    #[test]
    fn node_event_names_are_the_wire_names() {
        // The UI switches on these strings; a rename is a silent drop, so the
        // wire spelling is pinned rather than remembered.
        let names: Vec<&str> = node_events().iter().map(|e| e.event_name()).collect();
        assert_eq!(names, vec!["node_registered", "node_status", "node_log"]);
    }
}
