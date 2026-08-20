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
