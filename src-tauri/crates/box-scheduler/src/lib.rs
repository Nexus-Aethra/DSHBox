//! Framework-independent task state, locks, and persistence for DSH Box.

use box_foundation::{now_seconds, BoxPaths, BoxResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::{Arc, Mutex},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub kind: String,
    pub resource_keys: Vec<String>,
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
pub fn run_queued<F>(
    manager: &TaskManager,
    paths: &BoxPaths,
    notifier: std::sync::Arc<dyn TaskNotifier>,
    task_id: &str,
    work: F,
) where
    F: FnOnce(&TaskContext) -> BoxResult<()> + Send + 'static,
{
    loop {
        match manager.try_start(paths, task_id) {
            Ok(Some(task)) if task.status == "cancelled" => {
                notifier.log(task_id, "cancelled before execution");
                return;
            }
            Ok(Some(task)) => {
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
    };
    notifier.log(task_id, "worker started");
    let result = work(&context);
    let final_task = manager.finish(paths, task_id, &result).ok();
    if let Some(task) = &final_task {
        notifier.stage(task_id, &task.stage, task.progress);
    }
    let final_status = final_task
        .map(|task| task.status)
        .unwrap_or_else(|| "failed".to_owned());
    notifier.log(
        task_id,
        match final_status.as_str() {
            "succeeded" => "completed",
            "cancelled" => "cancelled after the active operation returned",
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
            // Only insert tasks that are not already tracked in memory.
            // This preserves the in-memory status of running tasks while
            // picking up tasks that were enqueued by another process.
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
            if matches!(task.status.as_str(), "queued" | "running" | "waiting_input") {
                task.status = "interrupted".to_owned();
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
            status: "queued".to_owned(),
            stage: "Queued".to_owned(),
            progress: 0,
            created_at: now_seconds(),
            started_at: None,
            finished_at: None,
            log_path: log_path.to_string_lossy().into_owned(),
            error: None,
            params,
            cancel_requested: false,
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
        if matches!(task.status.as_str(), "queued" | "running") {
            task.cancel_requested = true;
        }
        drop(state);
        self.persist(paths)
    }
    /// Removes a finished task record and its log file. Tasks that are
    /// queued, running, or waiting for input stay protected so resource
    /// locks and the concurrency counter never go stale.
    pub fn remove(&self, paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = match state.tasks.get(id) {
            Some(task) => task.clone(),
            None => return Ok(None),
        };
        if matches!(task.status.as_str(), "queued" | "running" | "waiting_input") {
            return Err("cannot delete a task that is still running".to_owned());
        }
        state.tasks.remove(id);
        drop(state);
        let _ = fs::remove_file(&task.log_path);
        self.persist(paths)?;
        Ok(Some(task))
    }
    /// Marks a queued task running only when its resource locks and the global
    /// concurrency limit permit execution.
    pub fn try_start(&self, paths: &BoxPaths, id: &str) -> BoxResult<Option<TaskRecord>> {
        let mut state = self.state.lock().map_err(|_| "task manager lock failed")?;
        let task = state.tasks.get(id).cloned().ok_or("task not found")?;
        if task.cancel_requested {
            let task = state.tasks.get_mut(id).ok_or("task not found")?;
            task.status = "cancelled".to_owned();
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
        task.status = "running".to_owned();
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
            task.status = "cancelled".to_owned();
            task.stage = "Cancellation requested".to_owned();
        } else if let Err(error) = result {
            task.status = "failed".to_owned();
            task.stage = "Failed".to_owned();
            task.error = Some(error.clone());
        } else {
            task.status = "succeeded".to_owned();
            task.stage = "Completed".to_owned();
            task.progress = 100;
        }
        let updated = task.clone();
        drop(state);
        self.persist(paths)?;
        Ok(updated)
    }
    pub fn resource_idle(&self, resource: &str) -> BoxResult<bool> {
        let state = self.state.lock().map_err(|_| "task manager lock failed")?;
        Ok(!state.active_resources.contains(resource)
            && !state.tasks.values().any(|task| {
                matches!(task.status.as_str(), "queued" | "running" | "waiting_input")
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

// ---- Client for the dshboxd daemon ----

/// Lightweight client that talks to the dshboxd daemon over the local
/// transport: Unix domain socket on Linux/macOS, named pipe on Windows.
/// The endpoint and token are read from the discovery file.
#[derive(Clone)]
pub struct TaskClient {
    endpoint: String,
    token: String,
}

impl TaskClient {
    pub fn connect(discovery: &serde_json::Value) -> Result<Self, String> {
        let endpoint = discovery["endpoint"]
            .as_str()
            .ok_or("discovery file missing endpoint")?
            .to_owned();
        let token = discovery["token"]
            .as_str()
            .ok_or("discovery file missing token")?
            .to_owned();
        Ok(Self { endpoint, token })
    }

    /// Send a JSON-line request and return the result field.
    #[cfg(unix)]
    fn call(&self, request: serde_json::Value) -> Result<serde_json::Value, String> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.endpoint)
            .map_err(|error| format!("cannot connect to daemon at {}: {error}", self.endpoint))?;
        let body = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        stream.write_all(body.as_bytes()).map_err(|error| error.to_string())?;
        stream.write_all(b"\n").map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|error| format!("daemon read error: {error}"))?;
        let response: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| format!("daemon response error: {error}"))?;
        if response["ok"].as_bool() != Some(true) {
            return Err(response["error"].as_str().unwrap_or("unknown daemon error").to_owned());
        }
        Ok(response["result"].clone())
    }

    #[cfg(windows)]
    fn call(&self, _request: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("dshboxd named-pipe transport is not yet implemented for Windows".to_owned())
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
        run_queued(&manager, &paths, notifier.clone(), &task.id, |context| {
            context.update("Working", 50);
            context.log("half way");
            context.check_cancelled()?;
            Ok(())
        });
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
        run_queued(&manager, &paths, notifier.clone(), &task.id, |_| {
            Err("boom".to_owned())
        });
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
        run_queued(&manager, &paths, notifier.clone(), &task.id, move |_| {
            ran.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
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
