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
    fmt, fs,
    sync::{Arc, Mutex},
};
use tracing::{error, info, warn};

/// A task's params are a small instruction, not a payload. The design caps
/// them at 4 KiB (section 3.5): deep-comparing a free-form JSON blob plus a
/// whole-scope upsert is what the millisecond-scale argument in section 6
/// rules out. Enforced at the enqueue boundary, never left to the caller.
pub const MAX_PARAMS_BYTES: usize = 4096;

/// The idempotency key is caller-supplied and short (section 3.5 says 64
/// *characters*, so CJK keys count by chars, not UTF-8 bytes).
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 64;

fn params_size(params: &serde_json::Value) -> usize {
    serde_json::to_vec(params)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

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
    /// The node that claimed this task no longer holds it: its helper session
    /// changed, or the node was removed. Deliberately not `Failed` (nothing
    /// about the work necessarily went wrong) and not `Interrupted` (the
    /// control plane did not go anywhere): the three want different actions
    /// from the user. Terminal, and never requeued on its own -- the build it
    /// lost may have written half an artifact.
    Miss,
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
            Self::Queued => vec![
                Self::Running,
                Self::Succeeded,
                Self::Failed,
                Self::Cancelled,
                Self::Interrupted,
                Self::Miss,
            ],
            Self::Running => vec![
                Self::Succeeded,
                Self::Failed,
                Self::Cancelled,
                Self::Interrupted,
                Self::Miss,
            ],
            Self::Failed => vec![Self::RollingBack],
            // A shutdown can land mid-rollback, so `Interrupted` is reachable
            // from here: without it a task whose rollback was in flight when the
            // daemon went away had no way out of `rolling_back` at all -- no
            // worker to advance it, and the shutdown sweep skipped it.
            Self::RollingBack => vec![Self::RolledBack, Self::Failed, Self::Interrupted],
            // Terminal states
            Self::Succeeded
            | Self::Cancelled
            | Self::Interrupted
            | Self::RolledBack
            | Self::Miss => {
                vec![]
            }
        }
    }

    /// Returns `true` if this is a terminal state (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Cancelled | Self::Interrupted | Self::RolledBack | Self::Miss
        )
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
            Self::Miss => write!(f, "miss"),
            Self::RollingBack => write!(f, "rolling_back"),
            Self::RolledBack => write!(f, "rolled_back"),
        }
    }
}

impl TaskState {
    /// Parse a status string, telling "unknown" apart from "failed".
    ///
    /// `From` has to answer with *some* state and answers `Failed` for anything
    /// it does not recognise -- correct on the wire, but a bad question to ask
    /// in a guard: a record whose status this build cannot spell would read as a
    /// finished failure and never be closed. Guards use this instead.
    pub fn from_known(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            "miss" => Self::Miss,
            // Two spellings reach this function: `transition_to` writes the
            // Display form (`rolling_back`) while serde writes the lowercase one
            // (`rollingback`) that `wire_format_tests` pins.
            "rolling_back" | "rollingback" => Self::RollingBack,
            "rolled_back" | "rolledback" => Self::RolledBack,
            _ => return None,
        })
    }
}

impl From<&str> for TaskState {
    fn from(s: &str) -> Self {
        Self::from_known(s).unwrap_or_else(|| {
            warn!(status = s, "unknown task status; reading it as failed");
            Self::Failed
        })
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
    let total: u64 = stages
        .iter()
        .map(|s| u64::from(s.duration_hint_secs.max(1)))
        .sum();
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
    // ---- Node placement (roadmap stage B1, design section 2.3) ----
    // All four default so a task written before node dispatch existed still
    // loads as a plain host-side task: no schema migration, no downgrade path.
    /// Which node the task is assigned to. `None` (and the legacy absent field)
    /// means the daemon's own local worker, i.e. "host".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub node_id: Option<String>,
    /// The node that claimed the task; `None` until `node_claim_task` writes it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub claimed_by: Option<String>,
    /// The helper instance that claimed it. A different current instance means
    /// the claim was lost, which is what `mark_missed` acts on (section 2.5).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub claim_session: Option<String>,
    /// Caller-supplied dedup key. The tuple is (kind, nodeId, key) and only
    /// active tasks dedup (section 3.5): a `miss` must not block a retry.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub idempotency_key: Option<String>,
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
    fn finished(&self, task_id: &str, status: &str, error: Option<&str>) {
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
        let _ = self
            .manager
            .set_stages(&self.paths, &self.task_id, planned.clone());
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
        let _ = self
            .manager
            .finish_stage(&self.paths, &self.task_id, id, None);
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
///
/// **That path cannot start from the contract's own flow, and no caller passes
/// `Some`.** When `work` fails the record is still `running`, and
/// `start_rollback` only accepts `failed -> rolling_back`, so it answers an error
/// and the rollback is skipped; a `work` closure that closes its own record can
/// still reach it, which is how one of the tests gets there, and nothing outside
/// a test does that. Making it properly reachable means deciding where the slot
/// is released relative to the rollback -- it is held today for exclusive access,
/// and `finish` is what releases it, while `is_live_record` does not count
/// `rolling_back` -- which is a change of its own. Until then the branch below is
/// kept honest rather than deleted, so that whoever wires it up does not also
/// inherit an invented status.
pub fn run_queued(
    manager: &TaskManager,
    paths: &BoxPaths,
    notifier: std::sync::Arc<dyn TaskNotifier>,
    task_id: &str,
    work: impl FnOnce(&TaskContext) -> BoxResult<()> + Send + 'static,
    rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>>,
) {
    // Wait for resource locks
    loop {
        match manager.try_start(paths, task_id) {
            Ok(Some(task)) => {
                // Anything that came back without reaching `Running` did not
                // start: it was cancelled, lost its claim, or was already
                // closed. Running the work anyway would spend minutes on a task
                // the box has already written off.
                let state = TaskState::from(task.status.as_str());
                if state != TaskState::Running {
                    // `try_start` answers `Some` from exactly two places: it
                    // admitted the task (`running`) or it closed a cancel
                    // request (`cancelled`). So this status is final by that
                    // contract -- and it is checked anyway, because the contract
                    // lives in another function and a caller-visible event must
                    // not rest on it.
                    notifier.log(task_id, &format!("{state} before execution"));
                    if is_finished_status(&task.status) {
                        notifier.finished(task_id, &task.status, task.error.as_deref());
                    }
                    return;
                }
                notifier.stage(task_id, &task.stage, task.progress);
                break;
            }
            Ok(None) => {
                // `try_start` answers `None` for two unrelated situations: the
                // gate is full, which is worth waiting for, and this record can
                // never be queued again, which is not. Only the caller can tell
                // them apart -- and waiting on the second leaks a thread that
                // spins every 100 ms for as long as the process lives.
                let record = manager.get(task_id).ok().flatten();
                if record.as_ref().is_some_and(|task| is_queued(&task.status)) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                // Nothing left to wait for -- and only some of these records have
                // a result to report. `finished` is what closes a task on the far
                // end, so it goes out for a record that really reached a final
                // state and for nothing else. A `running` record belongs to
                // another owner that will finish it, and a record that is gone
                // has no status at all: reporting the deleted case as
                // `cancelled` (the old fallback) put a result in the stream that
                // no code ever wrote.
                match record.as_ref() {
                    Some(task) if is_finished_status(&task.status) => {
                        notifier.log(task_id, "not queued anymore; nothing to run");
                        notifier.finished(task_id, &task.status, task.error.as_deref());
                    }
                    Some(task) => notifier.log(
                        task_id,
                        &format!("not queued anymore ({}); its owner reports it", task.status),
                    ),
                    None => notifier.log(task_id, "the record is gone; nothing to run"),
                }
                return;
            }
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
            let started = manager.start_rollback(paths, task_id);
            // A refusal used to be invisible: the branch simply did not run.
            // Nothing reaches that state from the contract's own flow (see
            // above), so if it ever happens the log is the only notice anyone
            // gets.
            if let Err(error) = &started {
                notifier.log(task_id, &format!("rollback not started: {error}"));
            }
            if let Ok(record) = started {
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
                // finish_rollback failed, so nothing has been reported yet and the
                // record is left where the rollback put it. Reporting here would
                // emit the task twice, because the standard finish below runs
                // anyway -- and it is the better reporter: `finish` refuses to
                // rewrite a record that already reached a state of its own
                // (`rolled_back` stays `rolled_back`) and moves a still
                // `rolling_back` one to `failed`, so whatever it reports is what
                // the record says at the moment of reporting. The hard-coded
                // `failed` that used to go out here announced a status the record
                // never held.
                notifier.log(
                    task_id,
                    "rollback finished unexpectedly; closing it as it stands",
                );
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
            TaskState::Miss => "the node that was running this no longer holds it",
            _ => "failed; inspect the error summary",
        },
    );
}

/// Whether a record is still waiting for a slot.
///
/// `try_start` is what refuses to admit a closed record and `run_queued` is what
/// stops waiting for one, so both have to answer the same question the same way.
/// Two copies of the predicate drift silently: the gate refuses while the loop
/// keeps polling, which is a thread that never ends. `from_known` rather than
/// `From` keeps a status this build cannot spell apart from `failed` -- it is
/// not queued either, but it is not a failure to report.
fn is_queued(status: &str) -> bool {
    TaskState::from_known(status) == Some(TaskState::Queued)
}

/// Whether a record's own status may be announced as a task's result.
///
/// `failed` counts: it is what `finish` writes for an error, and a caller
/// waiting on the task treats it as over -- even though the state machine still
/// allows a rollback from there, which reports again when it lands (with
/// `rolled_back`, or with this function's answer a second time if the rollback
/// itself failed).
///
/// Unknown statuses are *not* a result: this build cannot tell what such a
/// record is doing, and claiming it finished is the one thing it must not guess.
/// Public because the rule belongs to whoever emits the event, not only to
/// `run_queued`: `DaemonNotifier` forwards the status verbatim into
/// `TaskFinished`, so an invented one is not caught downstream.
pub fn is_finished_status(status: &str) -> bool {
    TaskState::from_known(status).is_some_and(|state| state.is_finished())
}

/// Whether a record still holds an execution slot and its resource locks.
///
/// `finish` releases them exactly once: `running` is the concurrency gate and
/// `active_resources` is what `resource_idle` reads, so releasing twice hands a
/// slot to a task that is not there. Anything this build cannot parse counts as
/// live, so a shutdown can still close it.
fn is_live_record(task: &TaskRecord) -> bool {
    // Only `Running` holds a slot: `try_start` is what takes it, in the same
    // critical section that moves the record to `Running`. A `Queued` task has
    // never entered the gate, so releasing on its behalf would raise the
    // concurrency limit by one every time it happens -- and a rollback releases
    // its own locks in `finish_rollback`. An unrecognised status counts as live:
    // leaking a slot is recoverable, wedging one is not.
    match TaskState::from_known(task.status.as_str()) {
        Some(TaskState::Running) => true,
        Some(_) => false,
        None => true,
    }
}

/// Whether a shutdown must close this record.
///
/// Everything that has not reached a final state, which includes `rolling_back`
/// -- a rollback in flight when the daemon stops has no worker to advance it and
/// no other way out, so skipping it would park the task forever. `waiting_input`
/// is live; `miss`, `failed` and the terminal states are history.
fn needs_shutdown_close(task: &TaskRecord) -> bool {
    match TaskState::from_known(task.status.as_str()) {
        Some(state) => !state.is_finished(),
        None => true,
    }
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
            if !needs_shutdown_close(task) {
                continue;
            }
            // Only rewrite when the record's own state allows the move. A status
            // this build cannot parse has no edge to `interrupted`, and writing
            // the fields anyway would relabel it on every start while leaving
            // `status` untouched -- a record that reads as interrupted and
            // reports as something else.
            if task.transition_to(TaskState::Interrupted).is_ok() {
                task.stage = "Interrupted by application restart".to_owned();
                task.finished_at = Some(now_seconds());
            } else {
                warn!(
                    task = %task.id,
                    status = %task.status,
                    "cannot close this task at start; leaving it as it is"
                );
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

    /// Read one record by id.
    ///
    /// Callers that only poll need this to tell "not ready yet" from "never going
    /// to be ready": `try_start` answers `None` for both.
    pub fn get(&self, id: &str) -> BoxResult<Option<TaskRecord>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "task manager lock failed")?
            .tasks
            .get(id)
            .cloned())
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
            node_id: None,
            claimed_by: None,
            claim_session: None,
            idempotency_key: None,
        };
        fs::write(
            &task.log_path,
            format!("[{}] queued {}\n", task.created_at, task.kind),
        )
        .map_err(|error| error.to_string())?;
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        state.tasks.insert(id, task.clone());
        // B7: this row, written under the same lock that admitted it.
        self.persist_row(&task)?;
        drop(state);
        Ok(task)
    }

    /// Enqueue a task assigned to a node (roadmap stage B1, design 2.3/3.5).
    ///
    /// Everything is validated before a record is written, and the dedup check
    /// shares the lock with the insert: two callers racing on the same
    /// (kind, nodeId, key) must not both enqueue. Only `queued`/`running` tasks
    /// dedup -- a finished, cancelled or `miss` record never blocks a retry.
    pub fn enqueue_for_node(
        &self,
        paths: &BoxPaths,
        kind: impl Into<String>,
        node_id: impl Into<String>,
        idempotency_key: Option<String>,
        mut resource_keys: Vec<String>,
        params: serde_json::Value,
    ) -> BoxResult<TaskRecord> {
        let kind = kind.into();
        let node_id = node_id.into();
        if kind.trim().is_empty() {
            return Err("task kind is required".to_owned());
        }
        // A node id is used to build the `node:<id>` resource key and, from
        // stage C on, a directory name. Reuse the registry's alphabet rather
        // than accepting `a|b` or `../../etc` here and refusing them there.
        if !box_foundation::is_safe_identifier(&node_id) {
            return Err("nodeId is not a safe identifier".to_owned());
        }
        if let Some(key) = &idempotency_key {
            let chars = key.chars().count();
            if chars == 0 || chars > MAX_IDEMPOTENCY_KEY_CHARS {
                return Err(format!(
                    "idempotencyKey must be 1..{MAX_IDEMPOTENCY_KEY_CHARS} characters"
                ));
            }
        }
        let size = params_size(&params);
        if size > MAX_PARAMS_BYTES {
            return Err(format!("params exceed {MAX_PARAMS_BYTES} bytes: {size}"));
        }
        // `nodeId` is assignment, not a lock scope; the node key belongs in
        // resourceKeys so two tasks for one node cannot claim the same slot.
        // The base-level exclusion of section 3.2 (`base:<baseId>`) is the
        // caller's to add -- it arrives with node_build_artifact, which this
        // slice does not implement yet. Keep it a set, not a pile.
        let node_key = format!("node:{node_id}");
        if !resource_keys.iter().any(|key| key == &node_key) {
            resource_keys.push(node_key);
        }

        // One predicate, reused by the fast pre-check and the decisive one.
        // Only active tasks dedup: a `miss` must not permanently block a retry.
        let is_duplicate = |existing: &&TaskRecord, key: &str| {
            existing.kind == kind
                && existing.node_id.as_deref() == Some(node_id.as_str())
                && existing.idempotency_key.as_deref() == Some(key)
                && matches!(
                    TaskState::from_known(&existing.status),
                    Some(TaskState::Queued) | Some(TaskState::Running)
                )
        };
        // Fast path before any log is written, so a dedup hit leaves no
        // orphan log file. The decisive check below repeats it under the same
        // lock as the insert, so two racing callers cannot both enqueue.
        if let Some(key) = &idempotency_key {
            let state = self.state.lock().map_err(|_| "task manager lock failed")?;
            if let Some(existing) = state.tasks.values().find(|t| is_duplicate(t, key)) {
                return Ok(existing.clone());
            }
        }

        let id = uuid::Uuid::new_v4().to_string();
        let log_path = paths.task_log(&id)?;
        fs::create_dir_all(log_path.parent().ok_or("task log has no parent")?)
            .map_err(|error| error.to_string())?;
        let task = TaskRecord {
            id: id.clone(),
            kind: kind.clone(),
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
            node_id: Some(node_id.clone()),
            claimed_by: None,
            claim_session: None,
            idempotency_key: idempotency_key.clone(),
        };
        fs::write(
            &task.log_path,
            format!("[{}] queued {} for {}\n", task.created_at, kind, node_id),
        )
        .map_err(|error| error.to_string())?;

        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        if let Some(key) = &idempotency_key {
            let existing = state.tasks.values().find(|t| is_duplicate(t, key)).cloned();
            if let Some(existing) = existing {
                // A concurrent caller won the race. The log we just wrote
                // belongs to nobody; remove it rather than leaving an orphan.
                drop(state);
                let _ = fs::remove_file(&task.log_path);
                return Ok(existing);
            }
        }
        state.tasks.insert(id, task.clone());
        self.persist_row(&task)?;
        drop(state);
        Ok(task)
    }

    pub fn request_cancel(&self, _paths: &BoxPaths, id: &str) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let current = TaskState::from(task.status.as_str());
        if current.is_active() {
            task.cancel_requested = true;
        }
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
            // Only close what can still be closed. A record that already reached
            // a final state (a lost claim, say) must not be relabelled as
            // "cancelled before execution" just because a flag arrived late.
            if task.transition_to(TaskState::Cancelled).is_err() {
                return Ok(None);
            }
            task.stage = "Cancelled before execution".to_owned();
            task.finished_at = Some(now_seconds());
            let updated = task.clone();
            // Write this row before releasing the lock: no stale whole-scope
            // snapshot can overwrite it (design section 3.3, review B7).
            self.persist_row(&updated)?;
            drop(state);
            return Ok(Some(updated));
        }
        // Only a queued task may be admitted. Anything else -- a record closed
        // by `mark_missed`, a rollback in progress, a finished task -- must not
        // take a slot: the transition below would be refused, yet the slot and
        // the resource keys would already be taken and nothing would give them
        // back. Returning `None` leaves the record untouched.
        if !is_queued(&task.status) {
            return Ok(None);
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
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
        Ok(Some(updated))
    }

    /// Replace the declared stage plan. Persisted with the record.
    pub fn set_stages(&self, _paths: &BoxPaths, id: &str, stages: Vec<TaskStage>) -> BoxResult<()> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        task.stages = stages;
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        let updated = task.clone();
        // B7: write only this row, while the lock is still held.
        self.persist_row(&updated)?;
        drop(state);
        Ok(())
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
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
        Ok(updated)
    }

    /// Close a task whose node no longer holds it.
    ///
    /// This is the one place a claim is given up. The execution slot and the
    /// resource locks are released here exactly once (the same accounting
    /// `finish` does), the record lands on `Miss`, and a second call changes
    /// nothing. The control plane calls it when a node re-registers without
    /// reporting the task it claimed, or when the node is removed.
    pub fn mark_missed(&self, id: &str, reason: &str) -> BoxResult<TaskRecord> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let (keys, live) = {
            let task = state.tasks.get(id).ok_or("task not found")?;
            (task.resource_keys.clone(), is_live_record(task))
        };
        if live {
            state.running = state.running.saturating_sub(1);
            for key in keys {
                state.active_resources.remove(&key);
            }
        }
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        if task.state().can_transition_to(&TaskState::Miss) {
            task.transition_to(TaskState::Miss).ok();
            task.stage = "Lost by its node".to_owned();
            task.error = Some(reason.to_owned());
            task.finished_at = Some(now_seconds());
        } else if task.state() != TaskState::Miss {
            warn!(
                task = id,
                status = %task.status,
                "ignoring miss for a task its own state does not allow"
            );
        }
        let updated = task.clone();
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
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
        // Releasing the execution slot and the resource locks is *not* the same
        // act as writing the result, and only the first is idempotent-safe to
        // run once: `running` is the concurrency gate and `active_resources` is
        // what `resource_idle` reads, so a second release hands a slot to a task
        // that is not there and leaves a resource idle while it is not.
        //
        // Two finishes really happen: a rollback releases its own locks and can
        // still fall through to here, and (from stage B on) a node that lost its
        // claim can report after the record was already closed.
        let (keys, live) = {
            let task = state.tasks.get(id).ok_or("task not found")?;
            (task.resource_keys.clone(), is_live_record(task))
        };
        if live {
            state.running = state.running.saturating_sub(1);
            for key in keys {
                state.active_resources.remove(&key);
            }
        }
        let task = state.tasks.get_mut(id).ok_or("task not found")?;
        let target = if task.cancel_requested {
            TaskState::Cancelled
        } else if result.is_err() {
            TaskState::Failed
        } else {
            TaskState::Succeeded
        };
        // The record is only rewritten when the state machine allows the move.
        // `Miss` has no outgoing edge and a finished task cannot become
        // `succeeded`, so a late report leaves the history alone instead of
        // inventing a result. `waiting_input` is a live status with no variant,
        // so it is written directly.
        let from = task.state();
        let waiting_input = task.status == "waiting_input";
        if waiting_input || from == target || from.can_transition_to(&target) {
            if waiting_input {
                task.status = target.to_string();
            } else {
                task.transition_to(target.clone()).ok();
            }
            task.finished_at = Some(now_seconds());
            match &target {
                TaskState::Cancelled => task.stage = "Cancellation requested".to_owned(),
                TaskState::Failed => {
                    task.stage = "Failed".to_owned();
                    task.error = match result {
                        Err(error) => Some(error.clone()),
                        Ok(()) => None,
                    };
                }
                _ => {
                    task.stage = "Completed".to_owned();
                    task.progress = 100;
                }
            }
        } else {
            warn!(
                task = id,
                status = %task.status,
                "ignoring a finish the task's own state does not allow"
            );
        }
        let updated = task.clone();
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
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
            // `status` is the serialised form, so it is parsed back rather than
            // compared to a hand-written list of strings: that list spelled one
            // value (`rolledback`) that `transition_to` never writes (it writes
            // `rolled_back`), so a rolled-back task was treated as open and
            // rewritten to `Interrupted`. Anything that is not live is skipped,
            // which is also what keeps `Miss` (added for lost claims) out of
            // this sweep.
            .filter(|task| needs_shutdown_close(task))
            .map(|task| task.id.clone())
            .collect();
        if open.is_empty() {
            return Ok(Vec::new());
        }
        // Daemon shutdown: every worker is about to die with this process, so the
        // counters and the lock set are cleared wholesale rather than per task.
        // This is the only caller that may assume that -- a runtime "close these
        // tasks" sweep would have to release per record, the way `finish` does.
        state.running = 0;
        state.active_resources.clear();
        let mut closed = Vec::new();
        // Collect the rows this sweep changed, so the durable write covers
        // exactly them instead of a whole-scope snapshot (B7).
        let mut rows = Vec::new();
        for id in open {
            let task = state.tasks.get_mut(&id).ok_or("task not found")?;
            // A rollback that was in flight is worth saying out loud: the
            // resource may be half-undone, and "Interrupted" alone would read
            // like an ordinary restart.
            let mid_rollback =
                TaskState::from_known(task.status.as_str()) == Some(TaskState::RollingBack);
            // Rewrite only what the state machine allows. A status this build
            // cannot parse has no edge to `interrupted`; writing the fields
            // anyway would relabel it while leaving `status` alone.
            if task.transition_to(TaskState::Interrupted).is_err() {
                warn!(
                    task = %task.id,
                    status = %task.status,
                    "cannot close this task; leaving it as it is"
                );
                continue;
            }
            task.stage = if mid_rollback {
                "Interrupted during rollback".to_owned()
            } else {
                "Interrupted".to_owned()
            };
            task.finished_at = Some(now_seconds());
            task.error = Some(reason.to_owned());
            rows.push(task.clone());
            closed.push(id);
        }
        self.persist_rows(&rows)?;
        drop(state);
        Ok(closed)
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
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
        Ok(updated)
    }

    /// Mark a rollback as completed (RollingBack → RolledBack) or failed
    /// (RollingBack → Failed with `rollback_error`). Releases resource locks.
    ///
    /// The release here is unconditional, unlike `finish`'s, because a rollback
    /// is supposed to still hold its slot for exclusive access -- but
    /// `is_live_record` does not count `rolling_back`, so a record whose slot
    /// was already released (by `finish`, the only way into this state today)
    /// gets it released a second time. That is part of the unfinished rollback
    /// wiring described on `run_queued`, not something to fix by guarding this
    /// release: the guard that looks right here would skip the release on the
    /// flow the design intends.
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
        // Write this row before releasing the lock: no stale whole-scope
        // snapshot can overwrite it (design section 3.3, review B7).
        self.persist_row(&updated)?;
        drop(state);
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

    /// Durably write exactly the rows this mutation produced, while the
    /// caller still holds the state lock. A whole-scope `persist()` takes a
    /// snapshot *after* the lock is released, so a concurrent writer's row
    /// can be overwritten by a stale snapshot -- design section 3.3 (review
    /// B7): a claim written in another thread would be lost, and after a
    /// restart the task would look like a running task with no claim.
    ///
    /// `upsert` never deletes ids absent from the batch, so writing one row
    /// cannot erase another process's records. Callers must pass the row as
    /// it stands after their own mutation, cloned while the lock is held.
    fn persist_row(&self, task: &TaskRecord) -> BoxResult<()> {
        self.store.upsert(std::slice::from_ref(task))
    }

    /// The batch form for a mutation that closes several tasks at once
    /// (`interrupt_running`). Same single-writer rule as `persist_row`.
    fn persist_rows(&self, tasks: &[TaskRecord]) -> BoxResult<()> {
        self.store.upsert(tasks)
    }

    /// Durably project the in-memory set through the collection: every
    /// record is insert-or-replaced by id, and no id is ever deleted here
    /// (cross-process rows are removed only via `remove`).
    ///
    /// Startup/restore and shutdown flushes legitimately want the whole set.
    /// A single-record mutation must use `persist_row` instead: taking the
    /// snapshot here, after the state lock is released, is the B7 race.
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
                "transitions": ["running", "succeeded", "failed", "cancelled", "interrupted", "miss"],
                "terminal": false,
                "active": true,
            },
            {
                "name": "running",
                "label": "Running",
                "description": "Task is executing with resource locks held",
                "transitions": ["succeeded", "failed", "cancelled", "interrupted", "miss"],
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
                "name": "miss",
                "label": "Missed",
                "description": "The node that claimed this task no longer holds it; not requeued automatically",
                "transitions": [],
                "terminal": true,
                "active": false,
            },
            {
                "name": "rolling_back",
                "label": "Rolling Back",
                "description": "Rollback is in progress (undoing partial changes from a failed task)",
                "transitions": ["rolled_back", "failed", "interrupted"],
                "terminal": false,
                // `is_active` says false: a rollback holds its locks on purpose,
                // but it is not a slot the queue may hand to another task.
                "active": false,
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
            None => {
                return Err("daemon response parse error: missing header/body boundary".to_string())
            }
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
        let response: serde_json::Value = serde_json::from_str(body_part)
            .map_err(|error| format!("daemon response error: {error}"))?;
        if response["ok"].as_bool() != Some(true) {
            return Err(response["error"]
                .as_str()
                .unwrap_or("unknown daemon error")
                .to_owned());
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
            assert!(
                record.finished_at.is_some(),
                "{} has no finish time",
                record.id
            );
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
        /// `(task id, status)` of every `finished` notification.
        finished: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl TaskNotifier for RecordingNotifier {
        fn stage(&self, task_id: &str, stage: &str, progress: u8) {
            self.stages
                .lock()
                .unwrap()
                .push((format!("{task_id}:{stage}"), progress));
        }
        fn log(&self, task_id: &str, line: &str) {
            self.logs.lock().unwrap().push(format!("{task_id}:{line}"));
        }
        fn finished(&self, task_id: &str, status: &str, _error: Option<&str>) {
            let mut finished = self.finished.lock().unwrap();
            // One task, one result. `finish_rollback` and the standard finish
            // path can both see the same record, and reporting in both places
            // made a single task announce twice -- measured, not imagined.
            assert!(
                !finished.iter().any(|(id, _)| id == task_id),
                "announced twice: {finished:?} then {status}"
            );
            finished.push((task_id.to_owned(), status.to_owned()));
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
        run_queued(
            &manager,
            &paths,
            notifier.clone(),
            &task.id,
            |context| {
                context.update("Working", 50);
                context.log("half way");
                context.check_cancelled()?;
                Ok(())
            },
            rollback,
        );
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
        run_queued(
            &manager,
            &paths,
            notifier.clone(),
            &task.id,
            |_| Err("boom".to_owned()),
            rollback,
        );
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
        run_queued(
            &manager,
            &paths,
            notifier.clone(),
            &task.id,
            move |_| {
                ran.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
            rollback,
        );
        assert!(!executed.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(manager.task(&task.id).unwrap().status, "cancelled");
        assert!(notifier
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("cancelled before execution")));
        assert_eq!(
            notifier.finished.lock().unwrap().clone(),
            vec![(task.id.clone(), "cancelled".to_owned())],
            "the cancel path reports the status the record carries"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// The rollback branch's report, which had no test: one task, one result,
    /// and the status is the one `finish_rollback` just wrote. Getting here
    /// needs a rollback the contract's own flow cannot start (see the doc on
    /// `run_queued`), so the work closes its own record first -- the only way in
    /// outside a store fault.
    #[test]
    fn a_completed_rollback_is_announced_once_with_the_records_status() {
        let paths = paths("rollback-report");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(&paths, "test", vec!["r".to_owned()], serde_json::json!({}))
            .unwrap();
        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rolled_back = ran.clone();
        let rollback: Option<Box<dyn FnOnce(&TaskContext) + Send + 'static>> =
            Some(Box::new(move |_: &TaskContext| {
                rolled_back.store(true, std::sync::atomic::Ordering::Relaxed);
            }));
        run_queued(
            &manager,
            &paths,
            notifier.clone(),
            &task.id,
            |context: &TaskContext| {
                // Close the record, so `start_rollback` finds `failed`.
                let _ = context.manager.finish(
                    &context.paths,
                    &context.task_id,
                    &Err("boom".to_owned()),
                );
                Err("boom".to_owned())
            },
            rollback,
        );

        assert!(
            ran.load(std::sync::atomic::Ordering::Relaxed),
            "the rollback ran"
        );
        assert_eq!(manager.task(&task.id).unwrap().status, "rolled_back");
        assert_eq!(
            notifier.finished.lock().unwrap().clone(),
            vec![(task.id.clone(), "rolled_back".to_owned())],
            "announced once, with the status the record holds"
        );
        assert!(
            manager.resource_idle("r").unwrap(),
            "and the resource came back"
        );
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
        ctx.plan_stages(&[
            ("clone", "Cloning", 20),
            ("build", "Building", 450),
            ("publish", "Publishing", 40),
        ]);
        ctx.stage_started("clone");
        ctx.stage_done("clone");
        ctx.stage_started("build");
        ctx.stage_failed("build", "pnpm run build failed; inspect prepare.log");
        let stages = manager.task(&task.id).unwrap().stages;
        let build = stages.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(build.state, StageState::Failed);
        assert!(build
            .failure_detail
            .as_deref()
            .unwrap_or("")
            .contains("pnpm run build"));
        let publish = stages.iter().find(|s| s.id == "publish").unwrap();
        assert_eq!(
            publish.state,
            StageState::Pending,
            "later stages must not look done"
        );
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
        assert!(
            install.failure_detail.is_none(),
            "a retry clears the previous detail"
        );
    }

    /// The overall figure is weighted by budget, so the old single bar keeps
    /// telling the truth for readers that do not know about stages.
    #[test]
    fn overall_progress_is_weighted_and_never_reads_as_frozen() {
        let stages = vec![
            TaskStage {
                id: "a".to_owned(),
                label: "a".to_owned(),
                state: StageState::Done,
                progress: 100,
                started_at: None,
                finished_at: None,
                duration_hint_secs: 100,
                attempts: 0,
                failure_detail: None,
            },
            TaskStage {
                id: "b".to_owned(),
                label: "b".to_owned(),
                state: StageState::Running,
                progress: 50,
                started_at: None,
                finished_at: None,
                duration_hint_secs: 300,
                attempts: 0,
                failure_detail: None,
            },
        ];
        // 100% of 100 plus 50% of 300, over 400 total: 62.5, rounded to 63.
        assert_eq!(overall_progress(&stages), 63);
        assert_eq!(overall_progress(&[]), 0);

        // The case that made the old bar look dead: a short opening stage at
        // 18% inside a 645 s total is ~0.5% of the whole, which truncation
        // reported as 0 while the clone was demonstrably running.
        let opening = vec![
            TaskStage {
                id: "clone".to_owned(),
                label: "c".to_owned(),
                state: StageState::Running,
                progress: 18,
                started_at: Some(1),
                finished_at: None,
                duration_hint_secs: 20,
                attempts: 0,
                failure_detail: None,
            },
            TaskStage {
                id: "build".to_owned(),
                label: "b".to_owned(),
                state: StageState::Pending,
                progress: 0,
                started_at: None,
                finished_at: None,
                duration_hint_secs: 625,
                attempts: 0,
                failure_detail: None,
            },
        ];
        assert_eq!(overall_progress(&opening), 1);

        // A stage that has only just started is still started work.
        let just_started = vec![TaskStage {
            id: "clone".to_owned(),
            label: "c".to_owned(),
            state: StageState::Running,
            progress: 0,
            started_at: Some(1),
            finished_at: None,
            duration_hint_secs: 20,
            attempts: 0,
            failure_detail: None,
        }];
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
        assert!(
            record.stages.is_empty(),
            "an old record has no stages, and the UI falls back"
        );
    }

    /// Drive a record to `target` without running the worker, so the sweep and
    /// finish paths can be exercised against a state only a node produces.
    fn force_state(manager: &TaskManager, id: &str, target: TaskState) {
        let mut state = manager.state.lock().unwrap();
        state
            .tasks
            .get_mut(id)
            .expect("the task exists")
            .transition_to(target)
            .expect("the transition is allowed");
    }

    /// `miss` is terminal and has no outgoing edge: a lost claim is not the
    /// same as a task waiting to be picked up again, and requeueing it would
    /// race the process that may still be writing the artifact.
    #[test]
    fn a_missed_task_is_terminal_and_is_not_requeued() {
        assert!(TaskState::Miss.is_terminal());
        assert!(TaskState::Miss.is_finished());
        assert!(!TaskState::Miss.is_active());
        assert!(
            TaskState::Miss.valid_transitions().is_empty(),
            "no way out of miss"
        );
        assert!(TaskState::Running.can_transition_to(&TaskState::Miss));
        assert!(TaskState::Queued.can_transition_to(&TaskState::Miss));
        assert!(!TaskState::Miss.can_transition_to(&TaskState::Running));
    }

    fn running(manager: &TaskManager) -> usize {
        manager.state.lock().unwrap().running
    }

    /// Giving up a claim releases the slot and the lock exactly once. `running`
    /// is the concurrency gate and `active_resources` is what `resource_idle`
    /// reads, so a second release would hand a slot to a task that is not there.
    #[test]
    fn mark_missed_releases_the_slot_and_the_lock_exactly_once() {
        let paths = paths("miss-release");
        let manager = TaskManager::json(&paths).unwrap();
        let first = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        let second = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n2".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert!(manager.try_start(&paths, &first.id).unwrap().is_some());
        assert!(manager.try_start(&paths, &second.id).unwrap().is_some());
        assert!(!manager.resource_idle("node:n1").unwrap());
        assert_eq!(running(&manager), 2);

        let missed = manager
            .mark_missed(&first.id, "the node came back without it")
            .unwrap();
        assert_eq!(missed.status, "miss");
        assert_eq!(
            missed.error.as_deref(),
            Some("the node came back without it")
        );
        assert!(
            manager.resource_idle("node:n1").unwrap(),
            "its lock is free"
        );
        assert_eq!(running(&manager), 1, "one slot back, not both");

        // Idempotent: a second call must not free the other task's slot.
        manager.mark_missed(&first.id, "again").unwrap();
        assert_eq!(running(&manager), 1, "a repeated miss releases nothing");
    }

    /// A late report from a node that lost its claim changes nothing: the record
    /// is history, and `mark_missed` already settled the accounting.
    #[test]
    fn finish_after_a_miss_changes_neither_the_record_nor_the_slots() {
        let paths = paths("finish-after-miss");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert!(manager.try_start(&paths, &task.id).unwrap().is_some());
        manager.mark_missed(&task.id, "lost").unwrap();
        assert_eq!(running(&manager), 0);

        let record = manager.finish(&paths, &task.id, &Ok(())).unwrap();
        assert_eq!(
            record.status, "miss",
            "a late success does not rewrite a lost task"
        );
        assert_eq!(record.progress, 0, "and it does not fake progress");
        assert_eq!(record.error.as_deref(), Some("lost"));
        assert_eq!(running(&manager), 0, "and it releases nothing twice");
    }

    /// A rollback that was in flight when the daemon stopped has no worker to
    /// advance it -- before this it had no way out of `rolling_back` at all --
    /// so the shutdown sweep must close it, and say that it was mid-rollback.
    #[test]
    fn interrupt_running_closes_a_rollback_that_was_in_flight() {
        let paths = paths("interrupt-rollback");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        force_state(&manager, &task.id, TaskState::Failed);
        force_state(&manager, &task.id, TaskState::RollingBack);

        let closed = manager.interrupt_running("the daemon shut down").unwrap();
        assert_eq!(
            closed,
            vec![task.id.clone()],
            "a half-done rollback is closed"
        );
        let record = manager
            .list()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == task.id)
            .unwrap();
        assert_eq!(record.status, "interrupted");
        assert_eq!(record.stage, "Interrupted during rollback");
    }

    /// Final states are history: a `miss` and a rolled-back task are left alone.
    #[test]
    fn interrupt_running_leaves_finished_tasks_untouched() {
        let paths = paths("interrupt-final");
        let manager = TaskManager::json(&paths).unwrap();
        let missed = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager.mark_missed(&missed.id, "lost").unwrap();
        let rolled = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n2".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        force_state(&manager, &rolled.id, TaskState::Failed);
        force_state(&manager, &rolled.id, TaskState::RollingBack);
        force_state(&manager, &rolled.id, TaskState::RolledBack);

        let closed = manager.interrupt_running("the daemon shut down").unwrap();
        assert!(closed.is_empty(), "nothing live to close: {closed:?}");
        let statuses: Vec<String> = manager
            .list()
            .unwrap()
            .into_iter()
            .map(|task| task.status)
            .collect();
        assert!(
            statuses.iter().any(|status| status == "miss"),
            "{statuses:?}"
        );
        assert!(
            statuses.iter().any(|status| status == "rolled_back"),
            "{statuses:?}"
        );
        assert!(
            statuses.iter().all(|status| status != "interrupted"),
            "{statuses:?}"
        );
    }

    /// Only a `Running` record holds a slot: `try_start` takes it in the same
    /// critical section that moves the record to `Running`. A queued task that
    /// loses its claim was never admitted, so closing it must not raise the
    /// concurrency limit -- stage B makes `queued -> miss` a normal path (a
    /// removed node), and one widened slot per removal would quietly double the
    /// gate.
    #[test]
    fn mark_missed_on_a_queued_task_leaves_the_gate_alone() {
        let paths = paths("miss-queued");
        let manager = TaskManager::json(&paths).unwrap();
        let first = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        let second = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n2".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert!(manager.try_start(&paths, &first.id).unwrap().is_some());
        assert!(manager.try_start(&paths, &second.id).unwrap().is_some());
        assert_eq!(running(&manager), 2, "the gate is full");

        let queued = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n3".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager
            .mark_missed(&queued.id, "its node was removed")
            .unwrap();
        assert_eq!(running(&manager), 2, "a queued task never took a slot");

        let third = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n4".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        assert!(
            manager.try_start(&paths, &third.id).unwrap().is_none(),
            "the gate did not widen"
        );
    }

    /// A record that was closed must not be admitted. `try_start` moves the
    /// record to `Running` while taking the slot; if that transition is refused,
    /// the slot and the resource keys would never be given back, and the worker
    /// would spend minutes on a task the box has already written off.
    #[test]
    fn try_start_does_not_admit_a_closed_task() {
        let paths = paths("admit-closed");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager
            .mark_missed(&task.id, "its node was removed")
            .unwrap();

        assert!(
            manager.try_start(&paths, &task.id).unwrap().is_none(),
            "a missed task is not started"
        );
        assert_eq!(running(&manager), 0, "and it takes no slot");
        assert!(
            manager.resource_idle("node:n1").unwrap(),
            "nor its resource"
        );
        let record = manager
            .list()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == task.id)
            .unwrap();
        assert_eq!(record.status, "miss", "and the record is untouched");
        assert_eq!(record.stage, "Lost by its node");
    }

    /// A worker must give up on a record that can never be queued again. Before
    /// this, `try_start` answered `None` for both "the gate is full" and "this
    /// task is closed", so the polling loop spun every 100 ms for as long as the
    /// process lived -- a leaked thread per closed task, and invisible to a test
    /// that only looks at `try_start`'s return value.
    #[test]
    fn run_queued_returns_instead_of_spinning_on_a_closed_task() {
        let paths = paths("spin-closed");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(
                &paths,
                "node-build-artifact",
                vec!["node:n1".to_owned()],
                serde_json::json!({}),
            )
            .unwrap();
        manager
            .mark_missed(&task.id, "its node was removed")
            .unwrap();

        let (done, waited) = std::sync::mpsc::channel();
        let worker_manager = manager.clone();
        let worker_paths = paths.clone();
        let worker_id = task.id.clone();
        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let worker_notifier = notifier.clone();
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran = executed.clone();
        std::thread::spawn(move || {
            run_queued(
                &worker_manager,
                &worker_paths,
                worker_notifier,
                &worker_id,
                move |_: &TaskContext| {
                    ran.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                },
                None,
            );
            let _ = done.send(());
        });

        assert!(
            waited
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok(),
            "run_queued has to return for a task that can never start"
        );
        let record = manager
            .list()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == task.id)
            .unwrap();
        assert_eq!(record.status, "miss", "and the record is left alone");
        assert!(
            !executed.load(std::sync::atomic::Ordering::Relaxed),
            "and the work never runs"
        );
        // The status handed to the far end is the one the record carries. An
        // invented one would close the task on a result nothing wrote.
        assert_eq!(
            notifier.finished.lock().unwrap().clone(),
            vec![(task.id.clone(), "miss".to_owned())],
            "and the miss is the status reported"
        );
        assert!(
            manager.resource_idle("node:n1").unwrap(),
            "and nothing is held"
        );
    }

    /// The predicate every emitter of a task's result depends on.
    #[test]
    fn is_finished_status_accepts_only_a_result() {
        for status in [
            "succeeded",
            "failed",
            "cancelled",
            "interrupted",
            "miss",
            "rolled_back",
            // The other spelling serde writes.
            "rolledback",
        ] {
            assert!(is_finished_status(status), "{status} carries a result");
        }
        // `failed` is a result even though the state machine still allows a
        // rollback from it; what must not count is everything still going, and
        // everything this build cannot parse.
        for status in [
            "queued",
            "running",
            "rolling_back",
            "rollingback",
            "waiting_input",
            "",
            "bogus",
        ] {
            assert!(!is_finished_status(status), "{status} is not a result");
        }
    }

    /// A record another owner has already admitted must not be announced as
    /// finished. It used to be: `try_start` answers `None` for every non-queued
    /// record, which includes `running`, and the loop reported whatever the
    /// status said -- so a task that was still executing reached the far end as
    /// finished and stopped being watched.
    #[test]
    fn run_queued_reports_nothing_for_a_record_another_owner_holds() {
        let paths = paths("run-owned-elsewhere");
        let manager = TaskManager::json(&paths).unwrap();
        let task = manager
            .enqueue(&paths, "test", vec!["r".to_owned()], serde_json::json!({}))
            .unwrap();
        assert!(manager.try_start(&paths, &task.id).unwrap().is_some());
        assert_eq!(running(&manager), 1, "the record holds a slot");

        let notifier = std::sync::Arc::new(RecordingNotifier::default());
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran = executed.clone();
        run_queued(
            &manager,
            &paths,
            notifier.clone(),
            &task.id,
            move |_| {
                ran.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
            None,
        );

        assert!(
            !executed.load(std::sync::atomic::Ordering::Relaxed),
            "the work is not run a second time"
        );
        assert!(
            notifier.finished.lock().unwrap().is_empty(),
            "and no result is invented for a record that has not finished"
        );
        assert!(
            notifier
                .logs
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("its owner reports it")),
            "the loop says why it stopped waiting"
        );
        assert_eq!(
            manager.task(&task.id).unwrap().status,
            "running",
            "and the record is untouched"
        );
        assert_eq!(running(&manager), 1, "and the slot stays the other owner's");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    // ---- Node placement (roadmap stage B1, design section 2.3/3.5) ----

    /// A task written before node dispatch existed has none of the four new
    /// fields. It must still deserialize as a plain host-side task: adding a
    /// field to a persisted record is not a migration.
    #[test]
    fn a_legacy_task_without_node_fields_still_loads() {
        let value = serde_json::json!({
            "id": "legacy", "kind": "test", "resourceKeys": [],
            "status": "succeeded", "stage": "Completed", "progress": 100,
            "createdAt": 1_700_000_000, "startedAt": null, "finishedAt": null,
            "logPath": "/tmp/x.log", "error": null, "params": {},
            "cancelRequested": false
        });
        let task: TaskRecord = serde_json::from_value(value).unwrap();
        assert_eq!(task.node_id, None);
        assert_eq!(task.claimed_by, None);
        assert_eq!(task.claim_session, None);
        assert_eq!(task.idempotency_key, None);
    }

    /// The four fields are camelCase on the wire and survive a round trip.
    #[test]
    fn node_fields_speak_camel_case_on_the_wire() {
        let task = TaskRecord {
            id: "t".to_owned(),
            kind: "node_build_artifact".to_owned(),
            resource_keys: vec!["node:n1".to_owned()],
            status: "queued".to_owned(),
            stage: "Queued".to_owned(),
            progress: 0,
            stages: Vec::new(),
            created_at: 1,
            started_at: None,
            finished_at: None,
            log_path: String::new(),
            error: None,
            params: serde_json::json!({}),
            cancel_requested: false,
            rollback_error: None,
            node_id: Some("n1".to_owned()),
            claimed_by: Some("n1".to_owned()),
            claim_session: Some("s-1".to_owned()),
            idempotency_key: Some("k-1".to_owned()),
        };
        let value = serde_json::to_value(&task).unwrap();
        for key in ["nodeId", "claimedBy", "claimSession", "idempotencyKey"] {
            assert!(value.get(key).is_some(), "{key} is on the wire: {value}");
        }
        assert_eq!(
            serde_json::from_value::<TaskRecord>(value).unwrap().node_id,
            task.node_id
        );
    }

    fn node_task(
        manager: &TaskManager,
        paths: &BoxPaths,
        kind: &str,
        node: &str,
        key: Option<String>,
        params: serde_json::Value,
    ) -> BoxResult<TaskRecord> {
        manager.enqueue_for_node(paths, kind, node, key, vec![], params)
    }

    #[test]
    fn enqueue_for_node_sets_assignment_and_adds_the_node_lock() {
        let paths = paths("node-enqueue");
        let manager = TaskManager::json(&paths).unwrap();
        let task = node_task(
            &manager,
            &paths,
            "node_build_artifact",
            "n1",
            None,
            serde_json::json!({ "baseId": "base-0123456789abcdef" }),
        )
        .unwrap();
        assert_eq!(task.node_id.as_deref(), Some("n1"));
        assert_eq!(task.status, "queued");
        assert_eq!(task.claimed_by, None);
        assert!(task.resource_keys.iter().any(|k| k == "node:n1"));
        assert_eq!(manager.store.load_all().unwrap()[0].node_id, task.node_id);
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// Params are an instruction, not a payload: the cap is enforced before
    /// any log or row exists (design section 3.5).
    #[test]
    fn params_over_4kib_are_refused_before_anything_is_written() {
        let paths = paths("node-params-cap");
        let manager = TaskManager::json(&paths).unwrap();
        let at_cap = serde_json::json!({ "blob": "x".repeat(MAX_PARAMS_BYTES - 12) });
        assert!(
            params_size(&at_cap) <= MAX_PARAMS_BYTES,
            "test fixture is at the cap"
        );
        assert!(node_task(&manager, &paths, "k", "n1", None, at_cap).is_ok());
        let over = serde_json::json!({ "blob": "x".repeat(MAX_PARAMS_BYTES) });
        let error = node_task(&manager, &paths, "k", "n1", None, over).unwrap_err();
        assert!(error.contains("exceed"), "{error}");
        assert_eq!(
            manager.list().unwrap().len(),
            1,
            "the refused call wrote nothing"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    #[test]
    fn an_empty_kind_node_or_overlong_key_is_refused() {
        let paths = paths("node-enqueue-shape");
        let manager = TaskManager::json(&paths).unwrap();
        assert!(node_task(&manager, &paths, "", "n1", None, serde_json::json!({})).is_err());
        assert!(node_task(&manager, &paths, "k", "", None, serde_json::json!({})).is_err());
        // The alphabet is the same one the node registry uses (PR #36).
        for bad_node in ["a|b", "../../etc", "node with space", "node/child"] {
            assert!(
                node_task(&manager, &paths, "k", bad_node, None, serde_json::json!({})).is_err(),
                "{bad_node:?} was accepted"
            );
        }
        assert!(node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some(String::new()),
            serde_json::json!({})
        )
        .is_err());
        assert!(node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("x".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1)),
            serde_json::json!({})
        )
        .is_err());
        // Section 3.5 says 64 *characters*: a 64-CJK key is 192 UTF-8 bytes
        // and must still be accepted. Counting bytes here would halve the
        // limit for a Chinese caller.
        let cjk_64 = "中".repeat(MAX_IDEMPOTENCY_KEY_CHARS);
        assert!(cjk_64.chars().count() == MAX_IDEMPOTENCY_KEY_CHARS);
        assert!(node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some(cjk_64),
            serde_json::json!({})
        )
        .is_ok());
        assert!(node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("中".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1)),
            serde_json::json!({})
        )
        .is_err());
        assert_eq!(
            manager.list().unwrap().len(),
            1,
            "only the valid CJK key wrote a record"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// A dedup hit must not leave a log file for the task that was never
    /// inserted: the first enqueue writes one log, the duplicate writes none.
    #[test]
    fn a_dedup_hit_leaves_no_orphan_log() {
        let paths = paths("node-idem-log");
        let manager = TaskManager::json(&paths).unwrap();
        let first = node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("key".to_owned()),
            serde_json::json!({}),
        )
        .unwrap();
        let duplicate = node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("key".to_owned()),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(duplicate.id, first.id);
        let log_dir = Path::new(&first.log_path).parent().unwrap();
        let logs: Vec<_> = fs::read_dir(log_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(logs.len(), 1, "only the first task has a log: {logs:?}");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// The dedup tuple is (kind, nodeId, key) and covers only active states:
    /// a `miss` must not block a retry (design section 3.5).
    #[test]
    fn idempotency_dedups_only_active_tasks_with_the_same_tuple() {
        let paths = paths("node-idem");
        let manager = TaskManager::json(&paths).unwrap();
        let first = node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("key".to_owned()),
            serde_json::json!({}),
        )
        .unwrap();
        let second = node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("key".to_owned()),
            serde_json::json!({ "different": 1 }),
        )
        .unwrap();
        assert_eq!(
            second.id, first.id,
            "active + same tuple returns the existing task"
        );
        assert_eq!(manager.list().unwrap().len(), 1);
        assert_ne!(
            node_task(
                &manager,
                &paths,
                "k",
                "n2",
                Some("key".to_owned()),
                serde_json::json!({})
            )
            .unwrap()
            .id,
            first.id
        );
        assert_ne!(
            node_task(
                &manager,
                &paths,
                "other",
                "n1",
                Some("key".to_owned()),
                serde_json::json!({})
            )
            .unwrap()
            .id,
            first.id
        );
        assert_ne!(
            node_task(&manager, &paths, "k", "n1", None, serde_json::json!({}))
                .unwrap()
                .id,
            first.id
        );
        manager.mark_missed(&first.id, "node went away").unwrap();
        assert_eq!(manager.task(&first.id).unwrap().status, "miss");
        let retry = node_task(
            &manager,
            &paths,
            "k",
            "n1",
            Some("key".to_owned()),
            serde_json::json!({}),
        )
        .unwrap();
        assert_ne!(
            retry.id, first.id,
            "a miss must not permanently block the key"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// The whole terminal half of section 3.5, not just `miss`: adding any
    /// finished state to the dedup set would make a retry silently return the
    /// old taskId and never run the work again. A mutation that adds Succeeded
    /// to `is_duplicate` must turn this red (review of PR #37).
    #[test]
    fn no_terminal_state_dedups_a_retry() {
        for status in [
            "succeeded",
            "failed",
            "cancelled",
            "interrupted",
            "rolled_back",
            "rolledback",
            "miss",
        ] {
            let label = format!("terminal-{status}");
            let paths = paths(&label);
            let manager = TaskManager::json(&paths).unwrap();
            let first = node_task(
                &manager,
                &paths,
                "k",
                "n1",
                Some("key".to_owned()),
                serde_json::json!({}),
            )
            .unwrap();
            // Put the record in the terminal state. The status string is
            // spelled by TaskState's Display, so each case uses the exact
            // wire form a real transition would have written.
            let target = TaskState::from_known(status).unwrap();
            {
                let mut state = manager.state.lock().unwrap();
                let task = state.tasks.get_mut(&first.id).unwrap();
                task.status = target.to_string();
                task.finished_at = Some(now_seconds());
            }
            manager.persist().unwrap();
            let retry = node_task(
                &manager,
                &paths,
                "k",
                "n1",
                Some("key".to_owned()),
                serde_json::json!({}),
            )
            .unwrap();
            assert_ne!(retry.id, first.id, "{status} must not block a retry");
            assert_eq!(
                manager.list().unwrap().len(),
                2,
                "{status} retry was enqueued"
            );
            let _ = fs::remove_dir_all(paths.runtime.unwrap());
        }
    }

    // ---- B7: single-row writes (design section 3.3, review of PR #37) ----

    /// A mutation of one task must not rewrite another task row from a stale
    /// in-memory snapshot. The store is shared with the desktop and, from the
    /// claim writer on, with other daemon threads: a whole-scope `persist()`
    /// after the lock is released can overwrite a claim that was just written.
    /// Here another writer advances task B in the store while this manager
    /// still holds B older copy; `update` on task A must leave B alone.
    #[test]
    fn a_single_row_write_does_not_roll_back_another_writer() {
        let paths = paths("b7-row-isolation");
        let manager = TaskManager::json(&paths).unwrap();
        let a = manager
            .enqueue(&paths, "a", vec![], serde_json::json!({}))
            .unwrap();
        let b = manager
            .enqueue(&paths, "b", vec![], serde_json::json!({}))
            .unwrap();
        // Another writer (a second manager over the same store) advances B
        // to a stage this manager has never seen.
        let other = TaskManager::json(&paths).unwrap();
        other.merge_from_disk(&paths).unwrap();
        other.update(&paths, &b.id, "claimed by node", 30).unwrap();
        let advanced = manager.store.load_all().unwrap();
        let advanced_b = advanced.iter().find(|t| t.id == b.id).unwrap();
        assert_eq!(advanced_b.stage, "claimed by node");

        // This manager in-memory copy of B is still the old one. Mutating A
        // and writing ONLY A row must not put that old copy back.
        manager.update(&paths, &a.id, "running", 50).unwrap();
        let rows = manager.store.load_all().unwrap();
        let stored_b = rows.iter().find(|t| t.id == b.id).unwrap();
        assert_eq!(
            stored_b.stage, "claimed by node",
            "B7: a whole-scope persist would have restored the stale copy"
        );
        let stored_a = rows.iter().find(|t| t.id == a.id).unwrap();
        assert_eq!(stored_a.stage, "running");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// The claim writer's close sites (`finish`, `mark_missed`) must also write
    /// only the row they closed. The task they do *not* touch is what proves it:
    /// another writer claims C, this manager still holds C older copy, and a
    /// whole-scope write from closing A or B would put that stale copy back --
    /// which after a restart reads as a running task no node admits to holding.
    #[test]
    fn closing_one_task_does_not_restore_another_tasks_stale_row() {
        let paths = paths("b7-close-isolation");
        let manager = TaskManager::json(&paths).unwrap();
        let a = manager
            .enqueue(&paths, "a", vec![], serde_json::json!({}))
            .unwrap();
        let b = manager
            .enqueue(&paths, "b", vec![], serde_json::json!({}))
            .unwrap();
        let c = manager
            .enqueue(&paths, "c", vec![], serde_json::json!({}))
            .unwrap();
        // Another writer claims C; nothing below touches C.
        let other = TaskManager::json(&paths).unwrap();
        other.merge_from_disk(&paths).unwrap();
        other.update(&paths, &c.id, "claimed by node", 30).unwrap();

        manager.try_start(&paths, &a.id).unwrap();
        manager.finish(&paths, &a.id, &Ok(())).unwrap();
        manager.mark_missed(&b.id, "node went away").unwrap();

        let rows = manager.store.load_all().unwrap();
        let row = |id: &str| rows.iter().find(|t| t.id == id).unwrap().clone();
        assert_eq!(row(&a.id).status, "succeeded");
        assert_eq!(row(&b.id).status, "miss");
        assert_eq!(row(&b.id).stage, "Lost by its node");
        assert_eq!(
            row(&c.id).stage,
            "claimed by node",
            "B7: closing A or B restored a stale copy of C"
        );
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }
}

#[cfg(test)]
mod wire_format_tests {
    use super::{task_state_machine_definition, TaskState};

    /// Every state this crate can produce, beside the spelling it takes on the
    /// wire. Each test below iterates this list, so a variant added to the enum
    /// cannot be left behind by one of them; and the literal is the contract --
    /// `domain.ts`'s `TaskStatus` and the agent plugin compare against these
    /// exact strings, so "it is lower case and parses back" would not be a pin.
    const ALL_STATES: [(TaskState, &str); 9] = [
        (TaskState::Queued, "queued"),
        (TaskState::Running, "running"),
        (TaskState::Succeeded, "succeeded"),
        (TaskState::Failed, "failed"),
        (TaskState::Cancelled, "cancelled"),
        (TaskState::Interrupted, "interrupted"),
        (TaskState::Miss, "miss"),
        (TaskState::RollingBack, "rollingback"),
        (TaskState::RolledBack, "rolledback"),
    ];

    fn name(state: &TaskState) -> String {
        state.to_string()
    }

    /// The spelling a caller sees on the wire, pinned here because a task poll
    /// is written against it. The Rust spelling is not it: a caller comparing
    /// against `Succeeded` matches nothing, and a finished task is then never
    /// recognised as finished.
    #[test]
    fn task_state_serialises_in_lower_case() {
        for (state, wire) in ALL_STATES.iter() {
            let encoded = serde_json::to_string(state).expect("a task state encodes");
            assert_eq!(encoded, format!("\"{wire}\""), "{state:?} on the wire");
        }
    }

    /// Both spellings that reach a record parse back to the state they came
    /// from: serde writes `rollingback`, `transition_to` writes `rolling_back`,
    /// and a lost claim adds `miss`. A reader that knows only one of them
    /// silently reclassifies a finished task.
    #[test]
    fn every_spelling_parses_back_to_its_state() {
        for (state, wire) in ALL_STATES.iter() {
            assert_eq!(
                TaskState::from_known(wire).map(|parsed| name(&parsed)),
                Some(name(state)),
                "{wire} parses back"
            );
            assert_eq!(
                TaskState::from_known(name(state).as_str()).map(|parsed| name(&parsed)),
                Some(name(state)),
                "{state:?} through Display"
            );
        }
    }

    /// `task_state_machine_definition` is served over RPC and is what a client
    /// renders. A state missing from it is a state no client can show; a flag
    /// that disagrees with the predicates is worse than no flag; and a
    /// `transitions` list that drifted from `valid_transitions` is a diagram
    /// that lies about what can follow what.
    #[test]
    fn the_advertised_state_machine_matches_the_enum() {
        let definition = task_state_machine_definition();
        let states = definition["states"].as_array().expect("states is an array");
        let advertised: Vec<String> = states
            .iter()
            .map(|state| {
                state["name"]
                    .as_str()
                    .expect("every state is named")
                    .to_owned()
            })
            .collect();
        let expected: Vec<String> = ALL_STATES.iter().map(|(state, _)| name(state)).collect();
        assert_eq!(
            advertised, expected,
            "the RPC's state list drifted from the enum"
        );

        for (state, _) in ALL_STATES.iter() {
            let label = name(state);
            let entry = states
                .iter()
                .find(|entry| entry["name"].as_str() == Some(label.as_str()))
                .expect("every state is advertised");
            assert_eq!(
                entry["terminal"].as_bool(),
                Some(state.is_terminal()),
                "{label} terminal"
            );
            assert_eq!(
                entry["active"].as_bool(),
                Some(state.is_active()),
                "{label} active"
            );
            let mut advertised_edges: Vec<String> = entry["transitions"]
                .as_array()
                .expect("transitions is an array")
                .iter()
                .map(|edge| edge.as_str().expect("an edge is a string").to_owned())
                .collect();
            let mut real_edges: Vec<String> = state.valid_transitions().iter().map(name).collect();
            advertised_edges.sort();
            real_edges.sort();
            assert_eq!(advertised_edges, real_edges, "{label} transitions");
        }
    }
}
