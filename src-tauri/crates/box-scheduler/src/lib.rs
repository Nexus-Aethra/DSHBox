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

use box_foundation::collection::Collection;
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

/// One declared step of a task, with its own progress.
///
/// A single overall bar cannot answer the question a waiting user actually asks
/// ("is it working, and on what?"). A task spends minutes inside one step —
/// `pnpm run build` alone measured 433-471 s — so the step needs its own bar,
/// its budget, and whether it is the one that failed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStage {
    /// Stable identifier (`build.web`), used for retry accounting and i18n.
    pub id: String,
    /// Human text the daemon writes, for when no translation matches the id.
    pub label: String,
    pub state: StageState,
    /// This stage's own 0-100, not the task's.
    pub progress: u8,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    /// Expected seconds. The UI turns elapsed/budget into a bar, so a slow step
    /// reads as slow instead of as a dead process.
    pub duration_hint_secs: u64,
    /// How many times this stage has been attempted; a retry is visible.
    pub attempts: u32,
    /// Where exactly a failed stage stopped, so the answer is not just "failed".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure_detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageState {
    /// Never started.
    #[default]
    Pending,
    Running,
    Done,
    /// Not applicable this run (e.g. a host-only step on a VM build).
    Skipped,
    Failed,
}

/// Overall 0-100 derived from the declared stages.
///
/// Weighted by budget rather than stage count: a 450 s build must not weigh the
/// same as a 15 s validation, or the single bar this replaces is exactly what
/// we would still be producing. An unfinished stage contributes its own
/// progress, so the overall bar moves whenever any stage observes movement.
fn overall_progress(stages: &[TaskStage]) -> u8 {
    let total: u64 = stages.iter().map(|s| u64::from(s.duration_hint_secs.max(1))).sum();
    if total == 0 {
        return 0;
    }
    let earned: u64 = stages
        .iter()
        .map(|stage| {
            let weight = u64::from(stage.duration_hint_secs.max(1));
            let fraction = match stage.state {
                StageState::Done | StageState::Skipped => 100u64,
                StageState::Failed | StageState::Running | StageState::Pending => {
                    u64::from(stage.progress)
                }
            };
            weight * fraction
        })
        .sum();
    // Rounded, not truncated: a 20 s stage inside a 645 s total reaches only
    // ~3% of the whole bar, and truncating kept it at 0% while work was
    // demonstrably happening. The old single bar's job is to move.
    let overall = ((earned + total / 2) / total).min(100) as u8;
    // A running stage that has begun still counts as started work, so the bar
    // does not sit at zero for the whole of a short opening step.
    let any_running = stages.iter().any(|s| s.state == StageState::Running);
    let any_done = stages.iter().any(|s| s.state == StageState::Done);
    if overall == 0 && any_running && !any_done {
        return 1;
    }
    overall
}

impl StageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
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
    /// Declared stages with their own progress. Empty for a task kind that does
    /// not declare any, and for records written before stages existed — the UI
    /// falls back to the single overall bar, so an old record still renders.
    #[serde(default)]
    pub stages: Vec<TaskStage>,
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

    /// Declare the stages this task will move through. Called once, early.
    ///
    /// `(id, label, budget_seconds)` — the budget is what turns "still going"
    /// into "63% of a ~4 minute step", which is the difference between a bar
    /// that informs and one that merely spins.
    pub fn plan_stages(&self, stages: &[(&str, &str, u64)]) {
        let planned: Vec<TaskStage> = stages
            .iter()
            .map(|(id, label, budget)| TaskStage {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
                state: StageState::Pending,
                progress: 0,
                started_at: None,
                finished_at: None,
                duration_hint_secs: *budget,
                attempts: 0,
                failure_detail: None,
            })
            .collect();
        let _ = self.manager.set_stages(&self.paths, &self.task_id, planned.clone());
        let _ = planned;
    }

    /// Notify the host that the per-stage picture changed, and keep the
    /// legacy summary fields in step with it.
    ///
    /// Every stage transition funnels through here: the manager already wrote
    /// the record, so this reads it back once and emits both the one-line
    /// summary an old client understands and the stage array the panel needs.
    /// Without it, stages would be persisted but never pushed, and the UI
    /// would show the first snapshot until the next poll.
    fn publish_stages(&self) {
        if let Ok(task) = self.manager.task(&self.task_id) {
            self.notifier
                .stage(&self.task_id, &task.stage, task.progress);
        }
    }

    /// Mark a stage started (or resumed, which bumps its attempt count).
    pub fn stage_started(&self, id: &str) {
        let _ = self.manager.begin_stage(&self.paths, &self.task_id, id);
        self.publish_stages();
    }

    /// Record observed progress inside a stage. Pass a milestone, not a guess.
    pub fn stage_progress(&self, id: &str, progress: u8) {
        let _ = self
            .manager
            .set_stage_progress(&self.paths, &self.task_id, id, progress);
        self.publish_stages();
    }

    /// Advance a stage from elapsed time against its budget, capped below 100.
    ///
    /// The cap is the point: a bar that reaches 100% on a timer lies, and a step
    /// this reports as 90% can still be stuck. Only `stage_done` says finished,
    /// so "at 90% and elapsed keeps climbing" is the honest signal for "hung".
    pub fn stage_tick(&self, id: &str) {
        let _ = self.manager.tick_stage(&self.paths, &self.task_id, id);
        self.publish_stages();
    }

    pub fn stage_done(&self, id: &str) {
        let _ = self.manager.finish_stage(&self.paths, &self.task_id, id, None);
        self.publish_stages();
    }

    /// Mark a stage failed, carrying where it stopped.
    pub fn stage_failed(&self, id: &str, detail: impl Into<String>) {
        let _ = self
            .manager
            .finish_stage(&self.paths, &self.task_id, id, Some(detail.into()));
        self.publish_stages();
    }

    pub fn stage_skipped(&self, id: &str) {
        let _ = self.manager.skip_stage(&self.paths, &self.task_id, id);
        self.publish_stages();
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
/// The share of a stage's budget that has elapsed, as a percentage that
/// stops at 90.
///
/// Pure on purpose: the clock is the only impure input, so a test can pin
/// the elapsed time instead of waiting for the wall clock.
fn stage_estimate(elapsed_secs: u64, budget_secs: u64) -> u8 {
    let budget = budget_secs.max(1);
    ((elapsed_secs.saturating_mul(90) / budget).min(90)) as u8
}

#[derive(Clone)]
pub struct TaskManager {
    state: Arc<Mutex<State>>,
    /// Durable projection of the in-memory set, keyed by task id. The
    /// concrete backend (SQLite via `box-store`, JSON files, in-memory) is
    /// chosen by the host at construction; see `box_foundation::collection`.
    store: Collection<TaskRecord>,
}

fn task_key(record: &TaskRecord) -> &str {
    record.id.as_str()
}

impl TaskManager {
    /// Create a manager persisting every mutation through `collection`. The
    /// in-memory map stays the working set; the collection is the durable
    /// projection (see `persist`).
    pub fn new(collection: Collection<TaskRecord>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            store: collection,
        }
    }

    /// File-backed manager over the legacy JSON layout
    /// (`<runtime>/state/tasks.json`).
    pub fn json(paths: &BoxPaths) -> BoxResult<Self> {
        let root = paths
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("state");
        Ok(Self::new(Collection::json_dir(root, "tasks", task_key)))
    }

    /// In-memory manager for hosts with no configured runtime and for
    /// tests that never cross manager instances.
    pub fn memory() -> Self {
        Self::new(Collection::memory("tasks", task_key))
    }

    /// Merge tasks from the durable store without overwriting in-memory
    /// tasks that are still queued or running. This lets the CLI and UI
    /// share the same task queue: the CLI enqueues and runs tasks, the UI
    /// picks up the new records without breaking its own in-flight work.
    pub fn merge_from_disk(&self, _paths: &BoxPaths) -> BoxResult<()> {
        let tasks = self.store.load_all()?;
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        for task in tasks {
            if !state.tasks.contains_key(&task.id) {
                state.tasks.insert(task.id.clone(), task);
            }
        }
        Ok(())
    }

    /// Load every stored task, mark active ones `interrupted` (daemon
    /// restart recovery), and adopt the result as the in-memory set.
    pub fn restore(&self, _paths: &BoxPaths) -> BoxResult<()> {
        let mut tasks = self.store.load_all()?;
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
        self.persist()
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
            stages: Vec::new(),
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
        self.persist()?;
        Ok(task)
    }

    pub fn request_cancel(&self, _paths: &BoxPaths, id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let current = TaskState::from(task.status.as_str());
        if current.is_active() {
            task.cancel_requested = true;
        }
        drop(state);
        self.persist()
    }

    /// Remove a finished task record and its log file. Active tasks
    /// (queued, running, rolling_back) stay protected so resource locks
    /// and the concurrency counter never go stale.
    pub fn remove(&self, _paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
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
        self.store.remove(id)?;
        Ok(Some(task))
    }

    /// Mark a queued task as running only when its resource locks and the
    /// global concurrency limit permit execution. Returns the updated task
    /// if the transition was applied, or `None` if the task is still waiting.
    pub fn try_start(&self, _paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get(id).cloned().ok_or("task not found")?;
        if task.cancel_requested {
            let task = state.tasks.get_mut(id).ok_or("task not found")?;
            task.transition_to(TaskState::Cancelled).ok();
            task.stage = "Cancelled before execution".to_owned();
            task.finished_at = Some(now_seconds());
            let updated = task.clone();
            drop(state);
            self.persist()?;
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
        self.persist()?;
        Ok(Some(updated))
    }

    /// Replace the declared stage plan. Persisted with the record.
    pub fn set_stages(&self, _paths: &BoxPaths, id: &str, stages: Vec<TaskStage>) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.stages = stages;
        drop(state);
        self.persist()
    }

    /// Start a stage, or bump its attempt count when it is already running or
    /// was left unfinished by a retry operation.
    pub fn begin_stage(&self, _paths: &BoxPaths, id: &str, stage_id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let Some(stage) = task.stages.iter_mut().find(|s| s.id == stage_id) else {
            return Err(format!("stage not declared: {stage_id}"));
        };
        if stage.state != StageState::Pending {
            stage.attempts = stage.attempts.saturating_add(1);
        }
        stage.state = StageState::Running;
        stage.started_at = Some(now_seconds());
        stage.finished_at = None;
        stage.failure_detail = None;
        drop(state);
        self.persist()
    }

    /// Record observed progress inside one stage (0-100).
    ///
    /// Monotonic within a run: a later, smaller number is a stale observation
    /// and is ignored rather than shown as the bar going backwards.
    pub fn set_stage_progress(
        &self,
        _paths: &BoxPaths,
        id: &str,
        stage_id: &str,
        progress: u8,
    ) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let Some(stage) = task.stages.iter_mut().find(|s| s.id == stage_id) else {
            return Err(format!("stage not declared: {stage_id}"));
        };
        stage.progress = progress.min(100).max(stage.progress);
        // Keep the legacy overall fields truthful for readers that only know
        // `stage`/`progress` (the command_surface gate, older clients).
        task.stage = stage.label.clone();
        task.progress = overall_progress(&task.stages);
        drop(state);
        self.persist()
    }

    /// Advance a running stage from elapsed time against its budget.
    ///
    /// Capped at 90: only `finish_stage` reports completion. A stage frozen at
    /// 90 with a rising elapsed time is the honest picture of a stuck step,
    /// while a timer that reaches 100 would claim success it cannot observe.
    pub fn tick_stage(&self, _paths: &BoxPaths, id: &str, stage_id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let Some(stage) = task.stages.iter_mut().find(|s| s.id == stage_id) else {
            return Err(format!("stage not declared: {stage_id}"));
        };
        if stage.state != StageState::Running {
            return Ok(());
        }
        let elapsed = now_seconds().saturating_sub(stage.started_at.unwrap_or(now_seconds()));
        let budget = stage.duration_hint_secs.max(1);
        let estimate = stage_estimate(elapsed, budget);
        stage.progress = stage.progress.max(estimate);
        task.stage = stage.label.clone();
        task.progress = overall_progress(&task.stages);
        drop(state);
        self.persist()
    }

    /// Close a stage as done, or as failed with the detail of where it stopped.
    pub fn finish_stage(
        &self,
        _paths: &BoxPaths,
        id: &str,
        stage_id: &str,
        failure: Option<String>,
    ) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let Some(index) = task.stages.iter().position(|s| s.id == stage_id) else {
            return Err(format!("stage not declared: {stage_id}"));
        };
        let now = now_seconds();
        {
            let stage = &mut task.stages[index];
            stage.finished_at = Some(now);
            match failure {
                Some(detail) => {
                    stage.state = StageState::Failed;
                    stage.failure_detail = Some(detail);
                }
                None => {
                    stage.state = StageState::Done;
                    stage.progress = 100;
                }
            }
        }
        task.stage = task.stages[index].label.clone();
        task.progress = overall_progress(&task.stages);
        drop(state);
        self.persist()
    }

    pub fn skip_stage(&self, _paths: &BoxPaths, id: &str, stage_id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let Some(stage) = task.stages.iter_mut().find(|s| s.id == stage_id) else {
            return Err(format!("stage not declared: {stage_id}"));
        };
        stage.state = StageState::Skipped;
        stage.finished_at = Some(now_seconds());
        task.progress = overall_progress(&task.stages);
        drop(state);
        self.persist()
    }
    pub fn update(
        &self,
        _paths: &BoxPaths,
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
        self.persist()?;
        Ok(updated)
    }

    /// Mark a task as finished, releasing all resource locks. The typed
    /// `TaskState` transition is validated before the status string is set.
    pub fn finish(
        &self,
        _paths: &BoxPaths,
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
        self.persist()?;
        Ok(updated)
    }

    /// Mark every task that has not reached a terminal state as `Interrupted`,
    /// releasing its resource locks. Returns the ids it closed.
    ///
    /// A daemon that goes down takes its tasks with it, and without this the
    /// record simply stops mid-line: the last thing written is a progress
    /// message, and the next start finds a task that is neither running nor
    /// finished. That reads exactly like a task that hung, and a container that
    /// was halfway through starting looks like a container that will not start
    /// at all. Naming the cause turns both into something a user can act on --
    /// and, because the locks are released, the retry that follows is not
    /// blocked behind a task that no longer exists.
    ///
    /// `Interrupted` rather than `Failed`: nothing about the task's own work
    /// went wrong. It was still doing fine when its supervisor went away, and a
    /// report that says "failed" sends the reader looking in the wrong place.
    pub fn interrupt_running(&self, reason: &str) -> BoxResult<Vec<String>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let open: Vec<String> = state
            .tasks
            .values()
            // `status` is the serialised (lower-case) form, so it is compared
            // as a string. `TaskState` is the same enum behind a Display impl,
            // and the Rust spelling matches none of the wire values.
            .filter(|task| !matches!(
                task.status.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "rolledback"
            ))
            .map(|task| task.id.clone())
            .collect();
        if open.is_empty() {
            return Ok(Vec::new());
        }
        state.running = 0;
        state.active_resources.clear();
        for id in &open {
            let task = state.tasks.get_mut(id).ok_or("task not found")?;
            task.transition_to(TaskState::Interrupted).ok();
            task.stage = "Interrupted".to_owned();
            task.finished_at = Some(now_seconds());
            task.error = Some(reason.to_owned());
        }
        drop(state);
        self.persist()?;
        Ok(open)
    }
    /// Transition a failed task into the `RollingBack` state. The resource
    /// locks are temporarily retained so the rollback has exclusive access.
    /// Returns the updated task record, or an error if the transition is
    /// invalid (e.g., the task is not in `Failed` state).
    pub fn start_rollback(&self, _paths: &BoxPaths, id: &str) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.transition_to(TaskState::RollingBack)
            .map_err(|e| format!("cannot start rollback: {e}"))?;
        task.stage = "Rolling back".to_owned();
        task.progress = 0;
        let updated = task.clone();
        drop(state);
        self.persist()?;
        Ok(updated)
    }

    /// Mark a rollback as completed (RollingBack → RolledBack) or failed
    /// (RollingBack → Failed with `rollback_error`). Releases resource locks.
    pub fn finish_rollback(
        &self,
        _paths: &BoxPaths,
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
        self.persist()?;
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

    /// Durably project the in-memory set through the collection: every
    /// record is insert-or-replaced by id, and no id is ever deleted here
    /// (cross-process rows are removed only via `remove`).
    pub fn persist(&self) -> BoxResult<()> {
        let records = self.list()?;
        self.store.upsert(&records)
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

    /// A TaskContext bound to one enqueued task, for exercising the stage API
    /// without running the worker.
    fn context(paths: &BoxPaths, manager: &TaskManager, task_id: &str) -> TaskContext {
        TaskContext {
            manager: manager.clone(),
            paths: paths.clone(),
            notifier: std::sync::Arc::new(NoopNotifier),
            task_id: task_id.to_owned(),
            profile_dir: None,
        }
    }

    struct NoopNotifier;
    impl TaskNotifier for NoopNotifier {
        fn stage(&self, _task_id: &str, _stage: &str, _progress: u8) {}
        fn log(&self, _task_id: &str, _line: &str) {}
    }
    fn paths(name: &str) -> BoxPaths {
        let root = env::temp_dir().join(format!("dsh-box-scheduler-{name}-{}", now_seconds()));
        BoxPaths {
            config: root.join("config.json"),
            runtime: Some(root),
        }
    }

    /// The point of `interrupt_running`: a task whose daemon went away is
    /// neither running nor finished, and that third state is what makes it
    /// look like a hang. It must land on `interrupted`, carry the reason, and
    /// -- the part that is easy to miss -- give its resource lock back, so the
    /// retry after the restart is not queued behind a task that no longer
    /// exists.
    #[test]
    fn interrupt_running_closes_open_tasks_and_frees_their_locks() {
        let paths = paths("interrupt");
        let manager = TaskManager::json(&paths).unwrap();
        manager
            .enqueue(
                &paths,
                "container-start",
                vec!["container:one".to_owned()],
                serde_json::json!({ "id": "one" }),
            )
            .unwrap();
        manager
            .enqueue(
                &paths,
                "container-start",
                vec!["container:two".to_owned()],
                serde_json::json!({ "id": "two" }),
            )
            .unwrap();
        assert!(!manager.resource_idle("container:one").unwrap());

        let closed = manager.interrupt_running("the daemon went away").unwrap();
        assert_eq!(closed.len(), 2, "both open tasks are closed: {closed:?}");

        for record in manager.list().unwrap() {
            assert_eq!(record.status, "interrupted", "{}", record.id);
            assert_eq!(record.stage, "Interrupted");
            assert_eq!(record.error.as_deref(), Some("the daemon went away"));
            assert!(record.finished_at.is_some(), "{} has no finish time", record.id);
        }
        assert!(manager.resource_idle("container:one").unwrap(), "lock held");
        assert!(manager.resource_idle("container:two").unwrap(), "lock held");

        // A second shutdown finds nothing left to close, and does not touch
        // what it already closed.
        assert!(manager.interrupt_running("again").unwrap().is_empty());
        assert_eq!(
            manager.list().unwrap()[0].error.as_deref(),
            Some("the daemon went away"),
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }
    #[test]
    fn queued_task_reserves_its_resource_for_short_transactions() {
        let paths = paths("resource");
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
        manager
            .enqueue(
                &paths,
                "runtime-install",
                vec!["runtime:latest".to_owned()],
                serde_json::json!({ "version": "latest" }),
            )
            .unwrap();
        let restored = TaskManager::json(&paths).unwrap();
        restored.restore(&paths).unwrap();
        assert_eq!(restored.list().unwrap()[0].status, "interrupted");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn only_two_distinct_resources_start_at_once() {
        let paths = paths("concurrency");
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
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
        let manager = TaskManager::json(&paths).unwrap();
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
    #[test]
    fn tick_caps_below_complete_and_finish_is_the_only_way_to_a_hundred() {
        let paths = paths("stage-tick");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(&paths, "template-pull", vec![], serde_json::json!({}))
            .unwrap();
        let ctx = context(&paths, &manager, &task.id);
        ctx.plan_stages(&[("build", "Building", 1)]);
        ctx.stage_started("build");
        // Far past the budget: still not complete. The estimator is a pure
        // function of elapsed time, so the wall clock cannot make this test
        // flaky in either direction.
        for _ in 0..3 {
            ctx.stage_tick("build");
        }
        let stages = manager.task(&task.id).unwrap().stages;
        let build = stages.iter().find(|s| s.id == "build").unwrap();
        assert!(
            build.progress <= 90,
            "a timer tops out below complete, got {}",
            build.progress
        );
        assert_eq!(build.state, StageState::Running);
        // And the estimator itself reaches the cap only when the budget is
        // spent many times over.
        assert_eq!(stage_estimate(0, 1), 0);
        assert_eq!(stage_estimate(1, 10), 9);
        assert_eq!(stage_estimate(10, 10), 90);
        assert_eq!(stage_estimate(10_000, 10), 90, "the cap is 90, not 100");
        ctx.stage_done("build");
        let stages = manager.task(&task.id).unwrap().stages;
        let build = stages.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(build.progress, 100);
        assert_eq!(build.state, StageState::Done);
    }

    /// A failed stage records where it stopped, and the stages after it stay
    /// pending rather than pretending to have succeeded.
    #[test]
    fn a_failed_stage_carries_its_detail_and_leaves_later_stages_pending() {
        let paths = paths("stage-fail");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(&paths, "template-pull", vec![], serde_json::json!({}))
            .unwrap();
        let ctx = context(&paths, &manager, &task.id);
        ctx.plan_stages(&[("clone", "Cloning", 20), ("build", "Building", 450), ("publish", "Publishing", 40)]);
        ctx.stage_started("clone");
        ctx.stage_done("clone");
        ctx.stage_started("build");
        ctx.stage_failed("build", "pnpm run build failed; inspect prepare.log");
        let stages = manager.task(&task.id).unwrap().stages;
        let build = stages.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(build.state, StageState::Failed);
        assert!(build.failure_detail.as_deref().unwrap_or("").contains("pnpm run build"));
        let publish = stages.iter().find(|s| s.id == "publish").unwrap();
        assert_eq!(publish.state, StageState::Pending, "later stages must not look done");
        assert_eq!(publish.progress, 0);
    }

    /// Retrying a stage is visible as a count, not as a silent restart.
    #[test]
    fn restarting_a_stage_bumps_its_attempt_count() {
        let paths = paths("stage-attempts");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(&paths, "template-pull", vec![], serde_json::json!({}))
            .unwrap();
        let ctx = context(&paths, &manager, &task.id);
        ctx.plan_stages(&[("install", "Installing", 120)]);
        ctx.stage_started("install");
        ctx.stage_failed("install", "network");
        ctx.stage_started("install");
        let stages = manager.task(&task.id).unwrap().stages;
        let install = stages.iter().find(|s| s.id == "install").unwrap();
        assert_eq!(install.attempts, 1);
        assert_eq!(install.state, StageState::Running);
        assert!(install.failure_detail.is_none(), "a retry clears the previous detail");
    }

    /// The overall figure is weighted by budget, so the old single bar keeps
    /// telling the truth for readers that do not know about stages.
    #[test]
    fn overall_progress_is_weighted_and_never_reads_as_frozen() {
        let stages = vec![
            TaskStage { id: "a".to_owned(), label: "a".to_owned(), state: StageState::Done, progress: 100, started_at: None, finished_at: None, duration_hint_secs: 100, attempts: 0, failure_detail: None },
            TaskStage { id: "b".to_owned(), label: "b".to_owned(), state: StageState::Running, progress: 50, started_at: None, finished_at: None, duration_hint_secs: 300, attempts: 0, failure_detail: None },
        ];
        // 100% of 100 plus 50% of 300, over 400 total: 62.5, rounded to 63.
        assert_eq!(overall_progress(&stages), 63);
        assert_eq!(overall_progress(&[]), 0);

        // The case that made the old bar look dead: a short opening stage at
        // 18% inside a 645 s total is ~0.5% of the whole, which truncation
        // reported as 0 while the clone was demonstrably running.
        let opening = vec![
            TaskStage { id: "clone".to_owned(), label: "c".to_owned(), state: StageState::Running, progress: 18, started_at: Some(1), finished_at: None, duration_hint_secs: 20, attempts: 0, failure_detail: None },
            TaskStage { id: "build".to_owned(), label: "b".to_owned(), state: StageState::Pending, progress: 0, started_at: None, finished_at: None, duration_hint_secs: 625, attempts: 0, failure_detail: None },
        ];
        assert_eq!(overall_progress(&opening), 1);

        // A stage that has only just started is still started work.
        let just_started = vec![
            TaskStage { id: "clone".to_owned(), label: "c".to_owned(), state: StageState::Running, progress: 0, started_at: Some(1), finished_at: None, duration_hint_secs: 20, attempts: 0, failure_detail: None },
        ];
        assert_eq!(overall_progress(&just_started), 1);
    }

    /// A record written before stages existed still loads and renders.
    #[test]
    fn a_record_without_stages_still_deserialises() {
        let json = serde_json::json!({
            "id": "t1", "kind": "template-pull", "resourceKeys": [],
            "status": "running", "stage": "Working", "progress": 40,
            "createdAt": 1, "startedAt": 2, "finishedAt": null,
            "logPath": "/tmp/t1.log", "error": null, "params": {},
            "cancelRequested": false
        });
        let record: TaskRecord = serde_json::from_value(json).unwrap();
        assert!(record.stages.is_empty(), "an old record has no stages, and the UI falls back");
    }
}

#[cfg(test)]
mod wire_format_tests {
    use super::TaskState;

    /// The spelling a caller sees on the wire, pinned here because a task
    /// poll is written against it. The Rust spelling is not it: a caller
    /// comparing against `Succeeded` matches nothing, and a finished task is
    /// then never recognised as finished.
    #[test]
    fn task_state_serialises_in_lower_case() {
        for (state, wire) in [
            (TaskState::Queued, "queued"),
            (TaskState::Running, "running"),
            (TaskState::Succeeded, "succeeded"),
            (TaskState::Failed, "failed"),
            (TaskState::Cancelled, "cancelled"),
            (TaskState::Interrupted, "interrupted"),
            (TaskState::RollingBack, "rollingback"),
            (TaskState::RolledBack, "rolledback"),
        ] {
            let encoded = serde_json::to_string(&state).expect("a task state encodes");
            assert_eq!(encoded, format!("\"{wire}\""), "{state:?} on the wire");
        }
    }
}
