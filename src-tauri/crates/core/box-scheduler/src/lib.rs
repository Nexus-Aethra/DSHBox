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

/// Which stored tasks a starting process may close as `interrupted`.
///
/// The sweep is an *ownership* question, not a convenience: the desktop and the
/// daemon share one task store, so a client that swept everything would close
/// the daemon's running tasks in that store — and a task that is still going
/// then reads as `interrupted` to every later reader, including the node that
/// holds its claim (review of PR #38, the cross-process half of B7). The daemon
/// owns every worker it starts; a client owns only the kinds it enqueues itself.
///
/// That is also where the current asymmetry sits: the daemon sweeps with
/// `Everything`, so a daemon start while a client is running one of *its* kinds
/// does close that client's row. It self-heals — the client's next mutation
/// writes the row back — and getting it exactly right needs an owner field on the
/// record rather than a kind list on both sides; see the design's open items.
#[derive(Clone, Debug)]
pub enum Sweep {
    /// Every stored task (the daemon, which owns the queue).
    Everything,
    /// Only these task kinds (a client that enqueues its own tasks).
    Only(Vec<String>),
}

impl Sweep {
    pub fn only<I, S>(kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Only(kinds.into_iter().map(Into::into).collect())
    }

    fn admits(&self, kind: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Only(kinds) => kinds.iter().any(|owned| owned == kind),
        }
    }
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
    /// Adds any stored row this manager does not already hold, and writes
    /// nothing: it is a read of the store into memory, so the row rule does not
    /// apply (writing back what was just read is the whole-scope write). It is
    /// also how the tests model a second writer.
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

    /// Load every stored task, close the ones this process owns as
    /// `interrupted` (restart recovery), and adopt the result as the in-memory
    /// set.
    ///
    /// `sweep` is the ownership boundary — see `Sweep` — because the desktop
    /// and the daemon share the store: a client that swept everything would
    /// close the daemon's running tasks on disk.
    ///
    /// Only the rows this sweep actually changed are written. The rest were just
    /// read from the store, so rewriting them would be a whole-scope write that
    /// undoes whatever another writer put there in the meantime (B7); the sweep's
    /// own rows are written through the same row writer every mutation uses.
    ///
    /// Returns how many records it closed.
    pub fn restore(&self, _paths: &BoxPaths, sweep: Sweep) -> BoxResult<usize> {
        let mut tasks = self.store.load_all()?;
        let mut closed = Vec::new();
        for task in &mut tasks {
            if !sweep.admits(&task.kind) || !needs_shutdown_close(task) {
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
                closed.push(task.id.clone());
            } else {
                warn!(
                    task = %task.id,
                    status = %task.status,
                    "cannot close this task at start; leaving it as it is"
                );
            }
        }
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        state.tasks = tasks
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        // Read the rows back from the adopted set rather than from the sweep's
        // own clones: what is written is then exactly what this process sees.
        let rows: Vec<TaskRecord> = closed
            .iter()
            .filter_map(|id| state.tasks.get(id).cloned())
            .collect();
        if !rows.is_empty() {
            self.persist_rows(&rows)?;
        }
        drop(state);
        Ok(rows.len())
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
    /// Remove one record. This is the only mutation that is not a row *write*:
    /// it deletes the id it was handed (`store.remove`) and touches no other
    /// row, so the B7 rule does not apply -- there is no snapshot to go stale.
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
    /// caller still holds the state lock.
    ///
    /// A whole-scope write takes its snapshot *after* the lock is released, so
    /// a concurrent writer's row is overwritten by a stale copy -- design
    /// section 3.3 (review B7): a claim written in another thread would be
    /// lost, and after a restart the task would look like a running task with
    /// no claim. This crate therefore has no whole-scope writer at all; the
    /// shape is not available to reach for.
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

    // There is deliberately no whole-scope writer. Every durable write goes
    // through `persist_row` (a mutation) or `persist_rows` (a sweep that closed
    // several rows); a method that snapshotted the whole in-memory set would
    // be the B7 race with a name, and a caller that did not know the history
    // would reach for it. `restore()` needs the whole set only to *read* it.
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
#[path = "lib_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "lib_wire_format_tests.rs"]
mod wire_format_tests;
