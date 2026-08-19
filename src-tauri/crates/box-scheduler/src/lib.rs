//! Framework-independent task state, locks, and persistence for DSH Box.
//!
//! # State machine
//!
//! Every task follows a lifecycle enforced by `TaskState`:
//!
//! ```text
//!                          ┌──────────────┐
//!                          │   Queued     │
//!                          └──────┬───────┘
//!                                 │
//!                    ┌────────────┼────────────┐
//!                    │            │            │
//!                    ▼            ▼            ▼
//!              ┌──────────┐ ┌──────────┐ ┌──────────────┐
//!              │ Running  │ │Cancelled │ │ Interrupted  │
//!              └─────┬────┘ └──────────┘ └──────────────┘
//!                    │
//!          ┌─────────┼─────────┐
//!          │         │         │
//!          ▼         ▼         ▼
//!    ┌──────────┐ ┌──────┐ ┌──────────┐
//!    │Succeeded │ │Failed│ │Cancelled │
//!    └──────────┘ └──┬───┘ └──────────┘
//!                    │
//!                    ▼
//!             ┌────────────┐
//!             │RollingBack │
//!             └──────┬─────┘
//!                    │
//!          ┌─────────┼─────────┐
//!          │                   │
//!          ▼                   ▼
//!    ┌──────────┐       ┌──────────┐
//!    │RolledBack│       │  Failed  │
//!    └──────────┘       │ (with    │
//!                       │ rollback │
//!                       │  error)  │
//!                       └──────────┘
//! ```
//!
//! Terminal states: `Succeeded`, `Cancelled`, `Interrupted`, `RolledBack`.
//! `Failed` is terminal only when `rollback_error` is set (rollback was
//! attempted and also failed). Otherwise `Failed` can transition to
//! `RollingBack` for automatic rollback.

use box_foundation::{now_seconds, BoxPaths, BoxResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs,
    sync::{Arc, Mutex},
};
use tracing::{error, info, warn};

/// Explicit task lifecycle states with validated transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    /// Task is enqueued and waiting for resource locks.
    Queued,
    /// Task is executing with resource locks held.
    Running,
    /// Task completed successfully.
    Succeeded,
    /// Task failed (may transition to `RollingBack` for automatic rollback).
    Failed,
    /// Task was cancelled by user request.
    Cancelled,
    /// Task was interrupted by daemon restart.
    Interrupted,
    /// Rollback is in progress (undoing partial changes from a failed task).
    RollingBack,
    /// Rollback completed successfully; the resource is clean.
    RolledBack,
}

impl TaskState {
    /// Valid transitions from this state. Returns `None` if the transition is
    /// not allowed.
    pub fn can_transition_to(&self, target: &TaskState) -> bool {
        self.valid_transitions().contains(target)
    }

    /// All valid next states from this state.
    pub fn valid_transitions(&self) -> Vec<TaskState> {
        match self {
            Self::Queued => vec![Self::Running, Self::Succeeded, Self::Failed, Self::Cancelled, Self::Interrupted],
            Self::Running => vec![Self::Succeeded, Self::Failed, Self::Cancelled, Self::Interrupted],
            Self::Failed => vec![Self::RollingBack],
            Self::RollingBack => vec![Self::RolledBack, Self::Failed],
            // Terminal states
            Self::Succeeded | Self::Cancelled | Self::Interrupted | Self::RolledBack => vec![],
        }
    }

    /// Returns `true` if this is a terminal state (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Cancelled | Self::Interrupted | Self::RolledBack)
    }

    /// Returns `true` if the task is still active (queued or running).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Returns `true` if the task is in a finished state (terminal or failed).
    pub fn is_finished(&self) -> bool {
        self.is_terminal() || matches!(self, Self::Failed)
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued => write!(f, "queued"),
            Self::Running => write!(f, "running"),
            Self::Succeeded => write!(f, "succeeded"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Interrupted => write!(f, "interrupted"),
            Self::RollingBack => write!(f, "rolling_back"),
            Self::RolledBack => write!(f, "rolled_back"),
        }
    }
}

impl From<&str> for TaskState {
    fn from(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            "rolling_back" => Self::RollingBack,
            "rolled_back" => Self::RolledBack,
            _ => Self::Failed, // unknown status → failed
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub kind: String,
    pub resource_keys: Vec<String>,
    /// The current state as a string. Use `state()` for the typed `TaskState`.
    pub status: String,
    pub stage: String,
    pub progress: u8,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub log_path: String,
    pub error: Option<String>,
    pub params: serde_json::Value,
    pub cancel_requested: bool,
    /// If rollback was attempted and failed, this holds the rollback error.
    /// The task remains in `Failed` state with both `error` (the original
    /// failure) and `rollback_error` (the rollback failure).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rollback_error: Option<String>,
}

impl TaskRecord {
    /// Returns the typed state from the `status` string.
    pub fn state(&self) -> TaskState {
        TaskState::from(self.status.as_str())
    }

    /// Transition the task to a new state, validating the transition.
    /// Returns an error if the transition is invalid.
    pub fn transition_to(&mut self, target: TaskState) -> Result<(), String> {
        let current = self.state();
        if current == target {
            return Ok(()); // idempotent
        }
        if !current.can_transition_to(&target) {
            return Err(format!(
                "invalid state transition: {} → {}",
                current, target
            ));
        }
        self.status = target.to_string();
        Ok(())
    }
}

pub trait TaskExecutor: Send + 'static {
    fn execute(self: Box<Self>, task: TaskRecord) -> BoxResult<()>;
}

/// How a running task reports progress to the host UI. The host implements
/// this trait (typically by forwarding to a tauri `Emitter`) so the scheduler
/// crate never needs to depend on a GUI framework.
pub trait TaskNotifier: Send + Sync {
    fn stage(&self, task_id: &str, stage: &str, progress: u8);
    fn log(&self, task_id: &str, line: &str);

    /// Called when a task reaches a terminal state. Default is a no-op for
    /// implementations that only care about stage/log; `DaemonNotifier`
    /// broadcasts a `TaskFinished` SSE event here.
    fn finished(
        &self,
        task_id: &str,
        status: &str,
        error: Option<&str>,
    ) {
        let _ = (task_id, status, error);
    }
}

/// Execution-time context handed to a task worker: cancel queries plus
/// progress and log reporting. Owned and cloneable so workers can move it
/// into background threads (log forwarding) or `'static` cancellation
/// closures without touching the GUI framework.
#[derive(Clone)]
pub struct TaskContext {
    pub manager: TaskManager,
    pub paths: BoxPaths,
    pub notifier: std::sync::Arc<dyn TaskNotifier>,
    pub task_id: String,
    /// Profile directory the task is scoped to. Used by workspace
    /// install handlers (`workspace:*`) and any other handler that
    /// needs to look up pnpm-workspace.yaml. `None` when the task
    /// runs outside a profile (e.g. host-side daemon maintenance).
    pub profile_dir: Option<std::path::PathBuf>,
}

impl TaskContext {
    pub fn cancelled(&self) -> bool {
        self.manager
            .task(&self.task_id)
            .map(|task| task.cancel_requested)
            .unwrap_or(true)
    }

    pub fn check_cancelled(&self) -> BoxResult<()> {
        if self.cancelled() {
            Err("task cancelled".to_owned())
        } else {
            Ok(())
        }
    }

    pub fn update(&self, stage: impl Into<String>, progress: u8) {
        let stage = stage.into();
        let _ = self
            .manager
            .update(&self.paths, &self.task_id, &stage, progress);
        self.notifier.stage(&self.task_id, &stage, progress);
    }

    /// Appends a timestamped line to the task's log file.
    pub fn append_log(&self, message: &str) {
        if let Ok(task) = self.manager.task(&self.task_id) {
            let line = format!("[{}] {message}\n", now_seconds());
            let _ = fs::OpenOptions::new()
                .append(true)
                .open(&task.log_path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        }
    }

    pub fn log(&self, message: &str) {
        self.append_log(message);
        self.notifier.log(&self.task_id, message);
    }
}

/// Generic task runner: polls until the resource locks and the concurrency
/// limit admit the task, runs the worker with a `TaskContext`, then persists
/// the final state. The host is expected to wrap this in a background thread.
///
/// When `work` returns an error and `rollback` is `Some`, the rollback closure
/// is called with the same `TaskContext` to undo partial changes. The rollback
/// is executed while resource locks are still held (via `start_rollback` /
/// `finish_rollback`), so the rollback has exclusive access to the resource.
pub fn run_queued(
    manager: &TaskManager,
    paths: &BoxPaths,
    notifier: std::sync::Arc<dyn TaskNotifier>,
    task_id: &str,
    work: impl FnOnce(&TaskContext) -> BoxResult<()> + Send + 'static,
    rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>>,
) where
{
    // Wait for resource locks
    loop {
        match manager.try_start(paths, task_id) {
            Ok(Some(task)) => {
                let state = TaskState::from(task.status.as_str());
                if state == TaskState::Cancelled || state == TaskState::Interrupted {
                    notifier.log(task_id, &format!("{state} before execution"));
                    notifier.finished(task_id, &task.status, task.error.as_deref());
                    return;
                }
                notifier.stage(task_id, &task.stage, task.progress);
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(_) => return,
        }
    }
    let context = TaskContext {
        manager: manager.clone(),
        paths: paths.clone(),
        notifier: notifier.clone(),
        task_id: task_id.to_owned(),
        profile_dir: None,
    };
    notifier.log(task_id, "worker started");
    info!(task_id, "worker began executing task body");
    let result = work(&context);
    match &result {
        Ok(()) => info!(task_id, "task body succeeded"),
        Err(error) => warn!(task_id, error = %error, "task body returned an error"),
    }

    // If work failed and we have a rollback, execute it while holding locks.
    // The rollback closure is `FnOnce` and must be consumed here.
    if result.is_err() {
        if let Some(rollback_fn) = rollback {
            // Mark as RollingBack so the UI shows the transition
            if let Ok(record) = manager.start_rollback(paths, task_id) {
                notifier.stage(task_id, &record.stage, record.progress);
                notifier.log(task_id, "rollback started");
                rollback_fn(&context);
                // finish_rollback releases the locks
                let rollback_outcome: BoxResult<()> = Ok(());
                if let Ok(record) = manager.finish_rollback(paths, task_id, &rollback_outcome) {
                    notifier.stage(task_id, &record.stage, record.progress);
                    notifier.log(task_id, "rollback completed");
                    notifier.finished(task_id, &record.status, record.error.as_deref());
                    return;
                }
                // finish_rollback failed — the task stays in Failed state
                notifier.log(task_id, "rollback finished unexpectedly");
                notifier.finished(task_id, "failed", None);
            }
        }
    }

    // Standard finish path (also releases locks for non-rollback failures)
    let final_task = manager.finish(paths, task_id, &result).ok();
    if let Some(task) = &final_task {
        notifier.stage(task_id, &task.stage, task.progress);
        notifier.finished(task_id, &task.status, task.error.as_deref());
    }
    let final_status = final_task
        .map(|task| task.status)
        .unwrap_or_else(|| "failed".to_owned());
    let final_state = TaskState::from(final_status.as_str());
    if matches!(final_state, TaskState::Failed) {
        error!(task_id, "task reached terminal failed state");
    } else {
        info!(task_id, status = %final_status, "task reached terminal state");
    }
    notifier.log(
        task_id,
        match final_state {
            TaskState::Succeeded => "completed",
            TaskState::Cancelled => "cancelled after the active operation returned",
            TaskState::RolledBack => "rolled back after failure",
            _ => "failed; inspect the error summary",
        },
    );
}

#[derive(Default)]
struct State {
    tasks: BTreeMap<String, TaskRecord>,
    active_resources: BTreeSet<String>,
    running: usize,
}

#[derive(Clone, Default)]
pub struct TaskManager {
    state: Arc<Mutex<State>>,
}

impl TaskManager {
    /// Merge tasks from the persisted state file without overwriting
    /// in-memory tasks that are still queued or running. This lets the CLI
    /// and UI share the same task queue: the CLI enqueues and runs tasks,
    /// the UI picks up the new records without breaking its own in-flight
    /// work.
    pub fn merge_from_disk(&self, paths: &BoxPaths) -> BoxResult<()> {
        let path = paths.tasks_state()?;
        if !path.exists() {
            return Ok(());
        }
        let tasks: Vec<TaskRecord> =
            serde_json::from_str(&fs::read_to_string(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        for task in tasks {
            if !state.tasks.contains_key(&task.id) {
                state.tasks.insert(task.id.clone(), task);
            }
        }
        Ok(())
    }

    pub fn restore(&self, paths: &BoxPaths) -> BoxResult<()> {
        let path = paths.tasks_state()?;
        if !path.exists() {
            return Ok(());
        }
        let mut tasks: Vec<TaskRecord> =
            serde_json::from_str(&fs::read_to_string(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        for task in &mut tasks {
            let current = TaskState::from(task.status.as_str());
            if current.is_active() || task.status == "waiting_input" {
                task.transition_to(TaskState::Interrupted).ok();
                task.stage = "Interrupted by application restart".to_owned();
                task.finished_at = Some(now_seconds());
            }
        }
        self.state
            .lock()
            .map_err(|_| "task manager lock failed")?
            .tasks = tasks
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        self.persist(paths)
    }

    pub fn list(&self) -> BoxResult<Vec<TaskRecord>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "task manager lock failed")?
            .tasks
            .values()
            .cloned()
            .collect())
    }

    pub fn task(&self, id: &str) -> BoxResult<TaskRecord> {
        self.state
            .lock()
            .map_err(|_| "task manager lock failed".to_owned())?
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| "task not found".to_owned())
    }

    pub fn enqueue(
        &self,
        paths: &BoxPaths,
        kind: impl Into<String>,
        resource_keys: Vec<String>,
        params: serde_json::Value,
    ) -> BoxResult<TaskRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        let log_path = paths.task_log(&id)?;
        fs::create_dir_all(log_path.parent().ok_or("task log has no parent")?)
            .map_err(|error| error.to_string())?;
        let task = TaskRecord {
            id: id.clone(),
            kind: kind.into(),
            resource_keys,
            status: TaskState::Queued.to_string(),
            stage: "Queued".to_owned(),
            progress: 0,
            created_at: now_seconds(),
            started_at: None,
            finished_at: None,
            log_path: log_path.to_string_lossy().into_owned(),
            error: None,
            params,
            cancel_requested: false,
            rollback_error: None,
        };
        fs::write(
            &task.log_path,
            format!("[{}] queued {}\n", task.created_at, task.kind),
        )
        .map_err(|error| error.to_string())?;
        self.state
            .lock()
            .map_err(|_| "task manager lock failed")?
            .tasks
            .insert(id, task.clone());
        self.persist(paths)?;
        Ok(task)
    }

    pub fn request_cancel(&self, paths: &BoxPaths, id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let current = TaskState::from(task.status.as_str());
        if current.is_active() {
            task.cancel_requested = true;
        }
        drop(state);
        self.persist(paths)
    }

    /// Remove a finished task record and its log file. Active tasks
    /// (queued, running, rolling_back) stay protected so resource locks
    /// and the concurrency counter never go stale.
    pub fn remove(&self, paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = match state.tasks.get(id) {
            Some(task) => task.clone(),
            None => return Ok(None),
        };
        let current = TaskState::from(task.status.as_str());
        if current.is_active() || task.status == "waiting_input" {
            return Err("cannot delete a task that is still active".to_owned());
        }
        state.tasks.remove(id);
        drop(state);
        let _ = fs::remove_file(&task.log_path);
        self.persist(paths)?;
        Ok(Some(task))
    }

    /// Mark a queued task as running only when its resource locks and the
    /// global concurrency limit permit execution. Returns the updated task
    /// if the transition was applied, or `None` if the task is still waiting.
    pub fn try_start(&self, paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get(id).cloned().ok_or("task not found")?;
        if task.cancel_requested {
            let task = state.tasks.get_mut(id).ok_or("task not found")?;
            task.transition_to(TaskState::Cancelled).ok();
            task.stage = "Cancelled before execution".to_owned();
            task.finished_at = Some(now_seconds());
            let updated = task.clone();
            drop(state);
            self.persist(paths)?;
            return Ok(Some(updated));
        }
        if state.running >= 2
            || task
                .resource_keys
                .iter()
                .any(|key| state.active_resources.contains(key))
        {
            return Ok(None);
        }
        state.running += 1;
        for key in &task.resource_keys {
            state.active_resources.insert(key.clone());
        }
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.transition_to(TaskState::Running).ok();
        task.stage = "Running".to_owned();
        task.started_at = Some(now_seconds());
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(Some(updated))
    }

    pub fn update(
        &self,
        paths: &BoxPaths,
        id: &str,
        stage: impl Into<String>,
        progress: u8,
    ) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.stage = stage.into();
        task.progress = progress.min(100);
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(updated)
    }

    /// Mark a task as finished, releasing all resource locks. The typed
    /// `TaskState` transition is validated before the status string is set.
    pub fn finish(
        &self,
        paths: &BoxPaths,
        id: &str,
        result: &BoxResult<()>,
    ) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let keys = state
            .tasks
            .get(id)
            .ok_or("task not found")?
            .resource_keys
            .clone();
        state.running = state.running.saturating_sub(1);
        for key in keys {
            state.active_resources.remove(&key);
        }
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.finished_at = Some(now_seconds());
        if task.cancel_requested {
            task.transition_to(TaskState::Cancelled).ok();
            task.stage = "Cancellation requested".to_owned();
        } else if let Err(error) = result {
            task.transition_to(TaskState::Failed).ok();
            task.stage = "Failed".to_owned();
            task.error = Some(error.clone());
        } else {
            task.transition_to(TaskState::Succeeded).ok();
            task.stage = "Completed".to_owned();
            task.progress = 100;
        }
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(updated)
    }

    /// Transition a failed task into the `RollingBack` state. The resource
    /// locks are temporarily retained so the rollback has exclusive access.
    /// Returns the updated task record, or an error if the transition is
    /// invalid (e.g., the task is not in `Failed` state).
    pub fn start_rollback(&self, paths: &BoxPaths, id: &str) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.transition_to(TaskState::RollingBack)
            .map_err(|e| format!("cannot start rollback: {e}"))?;
        task.stage = "Rolling back".to_owned();
        task.progress = 0;
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(updated)
    }

    /// Mark a rollback as completed (RollingBack → RolledBack) or failed
    /// (RollingBack → Failed with `rollback_error`). Releases resource locks.
    pub fn finish_rollback(
        &self,
        paths: &BoxPaths,
        id: &str,
        rollback_result: &BoxResult<()>,
    ) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let keys = state
            .tasks
            .get(id)
            .ok_or("task not found")?
            .resource_keys
            .clone();
        state.running = state.running.saturating_sub(1);
        for key in keys {
            state.active_resources.remove(&key);
        }
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        if let Err(rollback_error) = rollback_result {
            task.transition_to(TaskState::Failed).ok();
            task.stage = "Failed (rollback also failed)".to_owned();
            task.rollback_error = Some(rollback_error.clone());
            task.progress = 100;
        } else {
            task.transition_to(TaskState::RolledBack).ok();
            task.stage = "Rolled back".to_owned();
            task.progress = 100;
        }
        task.finished_at = Some(now_seconds());
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(updated)
    }

    pub fn resource_idle(&self, resource: &str) -> BoxResult<bool> {
        let state = self.state.lock().map_err(|_| "task manager lock failed")?;
        Ok(!state.active_resources.contains(resource)
            && !state.tasks.values().any(|task| {
                let current = TaskState::from(task.status.as_str());
                (current.is_active() || task.status == "waiting_input")
                    && task.resource_keys.iter().any(|key| key == resource)
            }))
    }

    pub fn persist(&self, paths: &BoxPaths) -> BoxResult<()> {
        let path = paths.tasks_state()?;
        fs::create_dir_all(path.parent().ok_or("task state has no parent")?)
            .map_err(|error| error.to_string())?;
        fs::write(
            path,
            serde_json::to_string_pretty(&self.list()?).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

/// Returns a JSON-serializable description of the task state machine,
/// including all valid states and their transitions.
pub fn task_state_machine_definition() -> serde_json::Value {
    serde_json::json!({
        "states": [
            {
                "name": "queued",
                "label": "Queued",
                "description": "Task is enqueued and waiting for resource locks",
                "transitions": ["running", "succeeded", "failed", "cancelled", "interrupted"],
                "terminal": false,
                "active": true,
            },
            {
                "name": "running",
                "label": "Running",
                "description": "Task is executing with resource locks held",
                "transitions": ["succeeded", "failed", "cancelled", "interrupted"],
                "terminal": false,
                "active": true,
            },
            {
                "name": "succeeded",
                "label": "Succeeded",
                "description": "Task completed successfully",
                "transitions": [],
                "terminal": true,
                "active": false,
            },
            {
                "name": "failed",
                "label": "Failed",
                "description": "Task failed (may transition to rolling_back for automatic rollback)",
                "transitions": ["rolling_back"],
                "terminal": false,
                "active": false,
            },
            {
                "name": "cancelled",
                "label": "Cancelled",
                "description": "Task was cancelled by user request",
                "transitions": [],
                "terminal": true,
                "active": false,
            },
            {
                "name": "interrupted",
                "label": "Interrupted",
                "description": "Task was interrupted by daemon restart",
                "transitions": [],
                "terminal": true,
                "active": false,
            },
            {
                "name": "rolling_back",
                "label": "Rolling Back",
                "description": "Rollback is in progress (undoing partial changes from a failed task)",
                "transitions": ["rolled_back", "failed"],
                "terminal": false,
                "active": true,
            },
            {
                "name": "rolled_back",
                "label": "Rolled Back",
                "description": "Rollback completed successfully; the resource is clean",
                "transitions": [],
                "terminal": true,
                "active": false,
            },
        ]
    })
}

// ---- Client for the dshboxd daemon ----

/// Lightweight client that talks to the dshboxd daemon over HTTP/1.1 on TCP.
/// The port and token are read from the discovery file.
#[derive(Clone)]
pub struct TaskClient {
    port: u16,
    token: String,
}

impl TaskClient {
    pub fn connect(discovery: &serde_json::Value) -> Result<Self, String> {
        let port = discovery["port"]
            .as_u64()
            .ok_or("discovery file missing port")?
            .try_into()
            .map_err(|_| "discovery port out of range")?;
        let token = discovery["token"]
            .as_str()
            .ok_or("discovery file missing token")?
            .to_owned();
        Ok(Self { port, token })
    }

    /// Send an HTTP POST /rpc request and return the result field.
    fn call(&self, request: serde_json::Value) -> Result<serde_json::Value, String> {
        use std::io::{BufReader, Read, Write};
        use std::net::TcpStream;

        let addr = format!("127.0.0.1:{}", self.port);
        let mut stream = TcpStream::connect(&addr)
            .map_err(|error| format!("cannot connect to daemon at {}: {error}", addr))?;

        let body = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        let request_line = format!(
            "POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            self.token,
            body
        );
        stream
            .write_all(request_line.as_bytes())
            .map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())?;

        let mut reader = BufReader::new(stream);
        let mut response_str = String::new();
        reader
            .read_to_string(&mut response_str)
            .map_err(|error| format!("daemon read error: {error}"))?;

        let mut boundary = None;
        for (i, _) in response_str.match_indices("\r\n\r\n") {
            boundary = Some(i + 4);
            break;
        }
        let boundary = match boundary {
            Some(pos) => pos,
            None => return Err("daemon response parse error: missing header/body boundary".to_string()),
        };

        let status_line = response_str[..boundary].lines().next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 200") {
            let body_part = &response_str[boundary..];
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(body_part);
            if let Ok(val) = parsed {
                let err = val["error"].as_str().unwrap_or("unknown daemon error");
                return Err(err.to_string());
            }
            return Err(format!("daemon returned: {}", status_line));
        }

        let body_part = &response_str[boundary..];
        let response: serde_json::Value =
            serde_json::from_str(body_part).map_err(|error| format!("daemon response error: {error}"))?;
        if response["ok"].as_bool() != Some(true) {
            return Err(response["error"].as_str().unwrap_or("unknown daemon error").to_owned());
        }
        Ok(response["result"].clone())
    }

    pub fn ping(&self) -> Result<serde_json::Value, String> {
        self.call(serde_json::json!({"token": self.token, "method": "ping"}))
    }

    pub fn enqueue(
        &self,
        kind: &str,
        resource_keys: Vec<String>,
        params: serde_json::Value,
    ) -> Result<TaskRecord, String> {
        let result = self.call(serde_json::json!({
            "token": self.token,
            "method": "enqueue_task",
            "kind": kind,
            "resource_keys": resource_keys,
            "params": params,
        }))?;
        serde_json::from_value(result).map_err(|error| format!("invalid task record: {error}"))
    }

    pub fn list(&self) -> Result<Vec<TaskRecord>, String> {
        let result = self.call(serde_json::json!({
            "token": self.token,
            "method": "list_tasks",
        }))?;
        serde_json::from_value(result).map_err(|error| format!("invalid task list: {error}"))
    }

    pub fn cancel(&self, id: &str) -> Result<(), String> {
        self.call(serde_json::json!({
            "token": self.token,
            "method": "cancel_task",
            "id": id,
        }))?;
        Ok(())
    }

    pub fn status(&self, id: &str) -> Result<TaskRecord, String> {
        let result = self.call(serde_json::json!({
            "token": self.token,
            "method": "task_status",
            "id": id,
        }))?;
        serde_json::from_value(result).map_err(|error| format!("invalid task record: {error}"))
    }

    pub fn update_progress(&self, id: &str, stage: &str, progress: u8) -> Result<(), String> {
        self.call(serde_json::json!({
            "token": self.token,
            "method": "update_progress",
            "id": id,
            "stage": stage,
            "progress": progress,
        }))?;
        Ok(())
    }

    pub fn finish(&self, id: &str, success: bool, error_msg: Option<&str>) -> Result<(), String> {
        self.call(serde_json::json!({
            "token": self.token,
            "method": "finish_task",
            "id": id,
            "success": success,
            "error_msg": error_msg,
        }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, path::Path};

    fn paths(name: &str) -> BoxPaths {
        let root = env::temp_dir().join(format!("dsh-box-scheduler-{name}-{}", now_seconds()));
        BoxPaths {
            config: root.join("config.json"),
            runtime: Some(root),
        }
    }

    #[test]
    fn queued_task_reserves_its_resource_for_short_transactions() {
        let paths = paths("resource");
        let manager = TaskManager::default();
        manager
            .enqueue(
                &paths,
                "container-start",
                vec!["container:one".to_owned()],
                serde_json::json!({ "id": "one" }),
            )
            .unwrap();
        assert!(!manager.resource_idle("container:one").unwrap());
        assert!(manager.resource_idle("container:two").unwrap());
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn restore_marks_incomplete_tasks_interrupted() {
        let paths = paths("restore");
        let manager = TaskManager::default();
        manager
            .enqueue(
                &paths,
                "runtime-install",
                vec!["runtime:latest".to_owned()],
                serde_json::json!({ "version": "latest" }),
            )
            .unwrap();
        let restored = TaskManager::default();
        restored.restore(&paths).unwrap();
        assert_eq!(restored.list().unwrap()[0].status, "interrupted");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn only_two_distinct_resources_start_at_once() {
        let paths = paths("concurrency");
        let manager = TaskManager::default();
        let first = manager
            .enqueue(
                &paths,
                "one",
                vec!["container:one".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        let second = manager
            .enqueue(
                &paths,
                "two",
                vec!["container:two".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        let third = manager
            .enqueue(
                &paths,
                "three",
                vec!["container:three".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(
            manager
                .try_start(&paths, &first.id)
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(
            manager
                .try_start(&paths, &second.id)
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        assert!(manager.try_start(&paths, &third.id).unwrap().is_none());
        manager.finish(&paths, &first.id, &Ok(())).unwrap();
        assert_eq!(
            manager
                .try_start(&paths, &third.id)
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn remove_deletes_finished_tasks_but_keeps_running_ones() {
        let paths = paths("remove");
        let manager = TaskManager::default();
        let finished = manager
            .enqueue(
                &paths,
                "finished",
                vec!["a".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager.finish(&paths, &finished.id, &Ok(())).unwrap();
        let running = manager
            .enqueue(
                &paths,
                "running",
                vec!["b".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager.try_start(&paths, &running.id).unwrap().unwrap();
        // Finished tasks can be removed, together with their log file.
        let removed = manager.remove(&paths, &finished.id).unwrap().unwrap();
        assert!(!Path::new(&removed.log_path).exists());
        assert!(manager.task(&finished.id).is_err());
        // Running tasks stay protected so locks never go stale.
        assert!(manager.remove(&paths, &running.id).is_err());
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn same_resource_waits_until_the_first_task_finishes() {
        let paths = paths("lock");
        let manager = TaskManager::default();
        let first = manager
            .enqueue(
                &paths,
                "one",
                vec!["runtime:latest".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        let second = manager
            .enqueue(
                &paths,
                "two",
                vec!["runtime:latest".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert!(manager.try_start(&paths, &first.id).unwrap().is_some());
        assert!(manager.try_start(&paths, &second.id).unwrap().is_none());
        manager.finish(&paths, &first.id, &Ok(())).unwrap();
        assert!(manager.try_start(&paths, &second.id).unwrap().is_some());
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[derive(Default)]
    struct RecordingNotifier {
        stages: std::sync::Mutex<Vec<(String, u8)>>,
        logs: std::sync::Mutex<Vec<String>>,
    }

    impl TaskNotifier for RecordingNotifier {
        fn stage(&self, task_id: &str, stage: &str, progress: u8) {
            self.stages
                .lock()
                .unwrap()
                .push((format!("{task_id}:{stage}"), progress));
        }
        fn log(&self, task_id: &str, line: &str) {
            self.logs
                .lock()
                .unwrap()
                .push(format!("{task_id}:{line}"));
        }
    }

    #[test]
    fn run_queued_executes_work_and_reports_progress() {
        let paths = paths("run");
        let manager = TaskManager::default();
        let task = manager
            .enqueue(&paths, "test", vec!["r".to_owned()], serde_json::json!({}))
            .unwrap();
        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>> = None;
        run_queued(&manager, &paths, notifier.clone(), &task.id, |context| {
            context.update("Working", 50);
            context.log("half way");
            context.check_cancelled()?;
            Ok(())
        }, rollback);
        let finished = manager.task(&task.id).unwrap();
        assert_eq!(finished.status, "succeeded");
        assert_eq!(finished.progress, 100);
        assert!(notifier
            .stages
            .lock()
            .unwrap()
            .iter()
            .any(|(stage, progress)| stage == &format!("{}:Working", task.id) && *progress == 50));
        assert!(notifier
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("half way")));
        assert!(notifier
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("completed")));
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn run_queued_reports_failure_and_releases_the_resource() {
        let paths = paths("run-fail");
        let manager = TaskManager::default();
        let task = manager
            .enqueue(&paths, "test", vec!["r".to_owned()], serde_json::json!({}))
            .unwrap();
        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>> = None;
        run_queued(&manager, &paths, notifier.clone(), &task.id, |_| {
            Err("boom".to_owned())
        }, rollback);
        assert_eq!(manager.task(&task.id).unwrap().status, "failed");
        assert_eq!(
            manager.task(&task.id).unwrap().error.as_deref(),
            Some("boom")
        );
        assert!(manager.resource_idle("r").unwrap());
        assert!(notifier
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("failed; inspect")));
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn run_queued_skips_work_for_a_cancelled_task() {
        let paths = paths("run-cancel");
        let manager = TaskManager::default();
        let task = manager
            .enqueue(&paths, "test", vec!["r".to_owned()], serde_json::json!({}))
            .unwrap();
        manager.request_cancel(&paths, &task.id).unwrap();
        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran = executed.clone();
        let rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>> = None;
        run_queued(&manager, &paths, notifier.clone(), &task.id, move |_| {
            ran.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }, rollback);
        assert!(!executed.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(manager.task(&task.id).unwrap().status, "cancelled");
        assert!(notifier
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("cancelled before execution")));
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }
}
