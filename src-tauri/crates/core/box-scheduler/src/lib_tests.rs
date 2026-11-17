//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

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
    /// A fresh runtime root per call. The suffix is a UUID, not
    /// `now_seconds()`: a wall-clock second is coarse enough that two tests
    /// in the same binary -- or a re-run inside the same second -- used to land
    /// on the SAME path. `TaskManager::json` reads every row back from
    /// `<runtime>/state/tasks.json`, so a leftover directory from the previous
    /// run is adopted as this test's starting state: the 1,582
    /// `dsh-box-scheduler-*` directories left in %TEMP% on this machine were
    /// the visible symptom, and a stale `running` row is a silent one. The
    /// UUID makes the directory unique per call, so no run can read or delete
    /// another's rows; each test still removes its own at the end.
    fn paths(name: &str) -> BoxPaths {
        let root = env::temp_dir().join(format!(
            "dsh-box-scheduler-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
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

    /// The sweep's rows must reach the store, and the test above cannot prove
    /// that: it reads `manager.list()`, which is the same manager's memory, so
    /// memory agreeing with itself is no evidence at all.
    ///
    /// This is the one multi-row write path, it is what a daemon shutdown uses,
    /// and it was unpinned: making `persist_rows` a no-op left the whole crate
    /// green while the disk kept `queued` records for work that had been written
    /// off -- which `restore()` then never closes, because a `queued` record is
    /// not something a restart sweep looks at (review of PR #38, B1).
    #[test]
    fn interrupt_running_writes_the_rows_it_closed() {
        let paths = paths("interrupt-durable");
        let manager = TaskManager::json(&paths).unwrap();
        let first = manager
            .enqueue(&paths, "one", vec![], serde_json::json!({}))
            .unwrap();
        let second = manager
            .enqueue(&paths, "two", vec![], serde_json::json!({}))
            .unwrap();

        assert_eq!(
            manager.interrupt_running("daemon stopped").unwrap().len(),
            2
        );

        // Read the store back through a manager that never saw the sweep. Only
        // the batch write can put these two rows there.
        let reader = TaskManager::json(&paths).unwrap();
        reader.merge_from_disk(&paths).unwrap();
        let rows = reader.list().unwrap();
        let row = |id: &str| rows.iter().find(|task| task.id == id).unwrap().clone();
        for id in [&first.id, &second.id] {
            let stored = row(id);
            assert_eq!(stored.status, "interrupted", "{id} was not written");
            assert_eq!(stored.error.as_deref(), Some("daemon stopped"), "{id}");
            assert!(stored.finished_at.is_some(), "{id} has no finish time");
        }
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
        assert_eq!(restored.restore(&paths, Sweep::Everything).unwrap(), 1);
        assert_eq!(restored.list().unwrap()[0].status, "interrupted");
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
    }

    /// A restart sweep closes only the rows whose workers this process started.
    ///
    /// The desktop shares one store with the daemon, so sweeping everything from
    /// there would write a daemon task that is still running off as
    /// `interrupted` on disk -- and the record would then disagree with the node
    /// that still holds it (review of PR #38, the cross-process half of B7).
    #[test]
    fn restore_sweeps_only_the_kinds_it_owns() {
        let paths = paths("restore-scope");
        let first = TaskManager::json(&paths).unwrap();
        let local = first
            .enqueue(&paths, "toolchain-install", vec![], serde_json::json!({}))
            .unwrap();
        let daemon_task = first
            .enqueue(&paths, "container-start", vec![], serde_json::json!({}))
            .unwrap();
        first.try_start(&paths, &daemon_task.id).unwrap();

        let client = TaskManager::json(&paths).unwrap();
        let closed = client
            .restore(&paths, Sweep::only(["toolchain-install"]))
            .unwrap();
        assert_eq!(closed, 1, "only the owned kind is closed");
        let rows = client.list().unwrap();
        let row = |id: &str| rows.iter().find(|task| task.id == id).unwrap().clone();
        assert_eq!(row(&local.id).status, "interrupted");
        assert_eq!(
            row(&daemon_task.id).status,
            "running",
            "a client must not write off a task the daemon is still running"
        );

        // The daemon's own start still closes everything it owns.
        let daemon = TaskManager::json(&paths).unwrap();
        assert_eq!(daemon.restore(&paths, Sweep::Everything).unwrap(), 1);
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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
        let _ = fs::remove_dir_all(paths.runtime.unwrap());
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

    fn one_stage() -> Vec<TaskStage> {
        vec![TaskStage {
            id: "compile".to_owned(),
            label: "Compile".to_owned(),
            state: StageState::Pending,
            progress: 0,
            started_at: None,
            finished_at: None,
            duration_hint_secs: 60,
            attempts: 0,
            failure_detail: None,
        }]
    }

    type B7Step = fn(&TaskManager, &BoxPaths, &str);

    fn prepare_nothing(_manager: &TaskManager, _paths: &BoxPaths, _id: &str) {}

    fn prepare_stages(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.set_stages(paths, id, one_stage()).unwrap();
    }

    fn prepare_started(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        prepare_stages(manager, paths, id);
        manager.begin_stage(paths, id, "compile").unwrap();
    }

    fn prepare_running(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.try_start(paths, id).unwrap();
    }

    fn prepare_failed(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        prepare_running(manager, paths, id);
        manager.finish(paths, id, &Err("boom".to_owned())).unwrap();
    }

    fn prepare_rolling_back(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        prepare_failed(manager, paths, id);
        manager.start_rollback(paths, id).unwrap();
    }

    fn act_set_stages(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.set_stages(paths, id, one_stage()).unwrap();
    }

    fn act_begin_stage(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.begin_stage(paths, id, "compile").unwrap();
    }

    fn act_set_stage_progress(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager
            .set_stage_progress(paths, id, "compile", 40)
            .unwrap();
    }

    fn act_tick_stage(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.tick_stage(paths, id, "compile").unwrap();
    }

    fn act_finish_stage(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.finish_stage(paths, id, "compile", None).unwrap();
    }

    fn act_skip_stage(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.skip_stage(paths, id, "compile").unwrap();
    }

    fn act_update(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.update(paths, id, "working", 50).unwrap();
    }

    fn act_try_start(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.try_start(paths, id).unwrap();
    }

    fn act_request_cancel(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.request_cancel(paths, id).unwrap();
    }

    fn act_finish(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.finish(paths, id, &Ok(())).unwrap();
    }

    fn act_start_rollback(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.start_rollback(paths, id).unwrap();
    }

    fn act_finish_rollback(manager: &TaskManager, paths: &BoxPaths, id: &str) {
        manager.finish_rollback(paths, id, &Ok(())).unwrap();
    }

    fn act_mark_missed(manager: &TaskManager, _paths: &BoxPaths, id: &str) {
        manager.mark_missed(id, "node went away").unwrap();
    }

    /// Every single-row mutation, not just the four the tests above drive.
    ///
    /// Each case prepares task A, has a second manager claim B, then runs one
    /// mutation on A and reads B back from the store. With the pre-fix
    /// `drop(state); self.persist()` left at that one site, B comes back
    /// `queued` -- which is why every converted site needs a case: a converted
    /// but unpinned site can be reverted silently (review of PR #38, N3).
    #[test]
    fn every_single_row_mutation_leaves_another_writers_row_alone() {
        let cases: Vec<(&str, B7Step, B7Step)> = vec![
            ("set_stages", prepare_nothing, act_set_stages),
            ("begin_stage", prepare_stages, act_begin_stage),
            (
                "set_stage_progress",
                prepare_started,
                act_set_stage_progress,
            ),
            ("tick_stage", prepare_started, act_tick_stage),
            ("finish_stage", prepare_started, act_finish_stage),
            ("skip_stage", prepare_stages, act_skip_stage),
            ("update", prepare_nothing, act_update),
            ("try_start", prepare_nothing, act_try_start),
            ("request_cancel", prepare_nothing, act_request_cancel),
            ("finish", prepare_running, act_finish),
            ("start_rollback", prepare_failed, act_start_rollback),
            ("finish_rollback", prepare_rolling_back, act_finish_rollback),
            ("mark_missed", prepare_running, act_mark_missed),
        ];
        for (name, prepare, act) in cases {
            let paths = paths(&format!("b7-all-{name}"));
            let manager = TaskManager::json(&paths).unwrap();
            let a = manager
                .enqueue(&paths, "a", vec![], serde_json::json!({}))
                .unwrap();
            let b = manager
                .enqueue(&paths, "b", vec![], serde_json::json!({}))
                .unwrap();
            prepare(&manager, &paths, &a.id);
            // Another writer's row lands after the preparation, so it is the
            // newest row in the store when the mutation runs.
            let other = TaskManager::json(&paths).unwrap();
            other.merge_from_disk(&paths).unwrap();
            other.update(&paths, &b.id, "claimed by node", 30).unwrap();
            act(&manager, &paths, &a.id);
            let reader = TaskManager::json(&paths).unwrap();
            reader.merge_from_disk(&paths).unwrap();
            let rows = reader.list().unwrap();
            let stored_b = rows.iter().find(|task| task.id == b.id).unwrap();
            assert_eq!(
                stored_b.stage, "claimed by node",
                "{name} rewrote another writer's row from a stale snapshot"
            );
            let _ = fs::remove_dir_all(paths.runtime.unwrap());
        }
    }
