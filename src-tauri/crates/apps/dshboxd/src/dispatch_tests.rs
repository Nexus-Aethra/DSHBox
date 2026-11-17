//! tests for dispatch.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

    //! Tests for the dispatch table; see `describe_container_rpc` below
    //! for the most thorough coverage. The `env_lock` helper at the
    //! crate level serialises every test that mutates `HOME` (or the
    //! daemon's config dir) so they cannot trample each other's
    //! persisted config files when cargo runs the suite in parallel.

use super::*;
    use crate::test_support::env_lock;
    use box_containers::{container_directory, host_pid_path};
    use box_foundation::{read_config, BoxConfig, BoxPaths};
    use std::{
        env, fs,
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    /// Builds a runtime directory with one container + a PID file pointing
    /// at a child the test owns. Returns the daemon state and the
    /// container id. The caller MUST hold `env_lock()` for the entire
    /// lifetime of the test (including `cleanup`) — see the module docs.
    fn setup(include_live_pid: bool) -> (DaemonState, String, PathBuf, PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let home = env::temp_dir().join(format!("dshboxd-dispatch-home-{stamp}"));
        let runtime = env::temp_dir().join(format!("dshboxd-dispatch-runtime-{stamp}"));
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        // Persist the runtime directory to a fake $HOME/.dsh-box/config.json.
        let config_dir = home.join(".dsh-box");
        fs::create_dir_all(&config_dir).unwrap();
        let config = BoxConfig {
            runtime_directory: Some(runtime.to_string_lossy().into_owned()),
            ..Default::default()
        };
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        // Same env var that box_foundation::read_config looks up.
        // SAFETY: tests are single-threaded within a process.
        unsafe { env::set_var("DSHBOX_CONFIG_DIR", &config_dir) };
        unsafe { env::set_var("HOME", &home) };

        let id = format!("container-{stamp}");
        let directory = container_directory(runtime.to_string_lossy().as_ref(), &id);
        fs::create_dir_all(directory.join("state")).unwrap();
        fs::create_dir_all(directory.join("profile/profiles")).unwrap();
        fs::write(
            directory.join("container.json"),
            serde_json::json!({
                "id": id,
                "name": "dsh-test",
                "version": "latest",
                "profile": "web",
                "template": "dsh-test",
            })
            .to_string(),
        )
        .unwrap();

        let pid = if include_live_pid {
            // Spawn a child whose PID we own; `kill -0` will succeed for
            // the lifetime of the test. Reaped at the end.
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let pid = child.id();
            // Stash the child for the test to reap later; storing into the
            // env is too awkward, so we leak the handle and rely on the
            // process being short-lived enough that the OS cleans up.
            Box::leak(Box::new(child));
            fs::write(
                host_pid_path(&box_containers::DshContainer {
                    id: id.clone(),
                    name: "dsh-test".into(),
                    version: "latest".into(),
                    profile: "web".into(),
                    template: Some("dsh-test".into()),
                    env: std::collections::BTreeMap::new(),
                    runtime: None,
                    directory: directory.to_string_lossy().into_owned(),
                    status: "running".into(),
                }),
                pid.to_string(),
            )
            .unwrap();
            Some(pid)
        } else {
            None
        };
        let _ = pid; // silence unused warning when include_live_pid is false

        // Build the DaemonState directly (avoids relying on the global
        // discovery file) using the same fields DaemonState::load wires up.
        let config = read_config().unwrap();
        let paths = BoxPaths::from_config(&config).unwrap();
        let events = Arc::new(crate::events::DaemonEvents::new());
        let state = DaemonState {
            manager: box_scheduler::TaskManager::memory(),
            paths: RwLock::new(paths),
            containers: Arc::new(ContainerManager::default()),
            resources: box_state::ResourceStateManager::default(),
            nodes: Arc::new(crate::nodes::NodeManager::load(
                &BoxPaths { config: std::path::PathBuf::from("/nonexistent"), runtime: None },
                String::new(),
                events.clone(),
            )),
            events,
            browser: std::sync::Mutex::new(crate::debug::Sessions::new()),
            artifacts: crate::node_artifacts::ArtifactIndex::memory(),
        };
        (state, id, home, runtime)
    }


    /// The generic enqueue route enforces the same 4 KiB cap. Deleting the
    /// check in `enqueue_task` must turn this red (review of PR #37).
    #[test]
    fn enqueue_task_refuses_params_over_4kib() {
        let events = Arc::new(crate::events::DaemonEvents::new());
        let empty = BoxPaths {
            config: PathBuf::from("/nonexistent"),
            runtime: None,
        };
        // enqueue writes a log file under runtime, so the normal-path half of
        // this test needs a real scratch root; the over-cap half is refused
        // before any path is touched.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let runtime = env::temp_dir().join(format!("dshboxd-enqueue-cap-{stamp}"));
        let _ = fs::create_dir_all(&runtime);
        let state = DaemonState {
            manager: box_scheduler::TaskManager::memory(),
            paths: RwLock::new(BoxPaths {
                config: PathBuf::from("/nonexistent"),
                runtime: Some(runtime.clone()),
            }),
            containers: Arc::new(ContainerManager::default()),
            resources: box_state::ResourceStateManager::default(),
            nodes: Arc::new(crate::nodes::NodeManager::load(
                &empty,
                String::new(),
                events.clone(),
            )),
            events,
            browser: std::sync::Mutex::new(crate::debug::Sessions::new()),
            artifacts: crate::node_artifacts::ArtifactIndex::memory(),
        };
        let over = serde_json::json!({
            "kind": "test",
            "params": { "blob": "x".repeat(box_scheduler::MAX_PARAMS_BYTES) },
        });
        let error = enqueue_task(&state, &over).unwrap_err();
        assert!(error.contains("exceed"), "{error}");
        assert!(state.manager.list().unwrap().is_empty(), "nothing was queued");
        // A request with no params still works (the default is an empty object).
        let ok = serde_json::json!({ "kind": "test" });
        assert!(enqueue_task(&state, &ok).is_ok());
        let _ = fs::remove_dir_all(runtime);
    }

    fn cleanup(home: &PathBuf, runtime: &PathBuf) {
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(runtime);
    }

    #[test]
    #[cfg(not(windows))]
    fn describe_container_marks_running_when_pid_alive() {
        // Linux/macOS only: the test spawns `sleep 30` and trusts
        // `kill -0` to probe the PID. Windows' tasklist-based probe has
        // PATH quirks that make it flaky in CI; the underlying production
        // behaviour (`pid_is_alive` via tasklist) is exercised in
        // `box_containers::tests::scan_containers_marks_running_*`.
        let _guard = env_lock();
        let (state, id, home, runtime) = setup(true);
        let response = describe_container_rpc(&state, &json!({ "id": id })).unwrap();
        // `describe_container_rpc` now returns `HandlerResult::Sync(json)`;
        // unwrap the inner value before asserting on its fields.
        let response = match response {
            HandlerResult::Sync(value) => value,
            HandlerResult::Async(_) => panic!("describe_container must be sync"),
        };
        assert_eq!(response["id"], id);
        assert_eq!(response["name"], "dsh-test");
        assert_eq!(response["version"], "latest");
        assert_eq!(response["profile"], "web");
        assert_eq!(response["template"], "dsh-test");
        assert_eq!(response["status"], "running");
        // URL is only set when the daemon actually started the host (no
        // running entry in the registry); `host_pid` is the live probe.
        assert!(response["url"].is_null());
        assert!(response["hostPid"].is_u64());
        // Extensions object is present (empty profile scan).
        assert!(response["extensions"].is_object());
        assert!(response["extensions"]["profiles"].is_array());
        cleanup(&home, &runtime);
    }

    #[test]
    #[cfg(not(windows))]
    fn describe_container_marks_stopped_when_pid_file_missing() {
        let _guard = env_lock();
        let (state, id, home, runtime) = setup(false);
        let response = describe_container_rpc(&state, &json!({ "id": id })).unwrap();
        let response = match response {
            HandlerResult::Sync(value) => value,
            HandlerResult::Async(_) => panic!("describe_container must be sync"),
        };
        assert_eq!(response["status"], "stopped");
        assert!(response["url"].is_null());
        assert!(response["hostPid"].is_null());
        cleanup(&home, &runtime);
    }

    #[test]
    fn harness_mirror_info_reports_an_absent_mirror_as_absent() {
        let _guard = env_lock();
        let (state, _id, home, runtime) = setup(false);
        let response = harness_mirror_info_rpc().unwrap();
        let response = match response {
            HandlerResult::Sync(value) => value,
            HandlerResult::Async(_) => panic!("harness_mirror_info must be sync"),
        };
        assert_eq!(response["present"], false);
        assert_eq!(response["cache"], true);
        assert_eq!(response["sizeBytes"], 0);
        // The path is named even when the mirror is not there, so a user
        // looking for it does not have to guess where it would live.
        let path = response["path"].as_str().unwrap();
        assert!(path.ends_with("harness-mirror.git"), "got {path}");
        assert!(path.contains(runtime.to_str().unwrap()), "got {path}");
        cleanup(&home, &runtime);
    }

    #[test]
    fn harness_mirror_info_reports_a_present_mirror_and_its_size() {
        let _guard = env_lock();
        let (_state, _id, home, runtime) = setup(false);
        // Build a usable mirror by hand: the info RPC must read what is there,
        // not require a pull to have created it.
        let mirror = box_runtime::mirror::mirror_path(&runtime);
        let never = || false;
        box_runtime::mirror::ensure_bare_mirror(
            &runtime,
            "https://example.invalid/repo.git",
            &box_runtime::mirror::MirrorCallbacks::new(&never),
        )
        .unwrap();
        let response = harness_mirror_info_rpc().unwrap();
        let response = match response {
            HandlerResult::Sync(value) => value,
            HandlerResult::Async(_) => panic!("harness_mirror_info must be sync"),
        };
        assert_eq!(response["present"], true);
        assert!(response["sizeBytes"].as_u64().unwrap() > 0);
        cleanup(&home, &runtime);
    }

    #[test]
    fn describe_container_answers_a_traversal_shape_with_an_honest_miss() {
        let _guard = env_lock();
        let (state, _id, home, runtime) = setup(false);
        // The join gate refuses path-shaped references before any path is
        // built; the name fallback then answers the same way an unknown name
        // does. No error text claims the reference was malformed, because a
        // legal name can carry those characters.
        let err = describe_container_rpc(&state, &json!({ "id": "../etc/passwd" })).unwrap_err();
        assert!(err.contains("no container named"), "got: {err}");
        cleanup(&home, &runtime);
    }

    #[test]
    fn describe_container_returns_error_for_unknown_id() {
        let _guard = env_lock();
        let (state, _id, home, runtime) = setup(false);
        let err =
            describe_container_rpc(&state, &json!({ "id": "container-missing" })).unwrap_err();
        // The wording moved with the shared resolver: an unknown reference is
        // still an error, but it now names the containers that do exist, which
        // is what ends an agent's guessing. Asserting the old literal would
        // pin the behaviour this wiring exists to replace.
        assert!(err.contains("no container named"), "got: {err}");
        assert!(
            err.contains("dsh-test"),
            "the diagnostic names what exists: {err}"
        );
        cleanup(&home, &runtime);
    }

    /// Write a minimal harness package: a `package.json` plus a source file whose
    /// declarations the scanner reads.
    fn write_graph_package(directory: &Path, name: &str, dependencies: &[&str], source: &str) {
        fs::create_dir_all(directory.join("src")).unwrap();
        let mut manifest = serde_json::json!({ "name": name, "version": "1.0.0" });
        if !dependencies.is_empty() {
            let mut map = serde_json::Map::new();
            for dependency in dependencies {
                map.insert((*dependency).to_owned(), serde_json::json!("workspace:^"));
            }
            manifest["dependencies"] = serde_json::Value::Object(map);
        }
        fs::write(
            directory.join("package.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("src").join("index.ts"), source).unwrap();
    }

    #[test]
    fn plugin_dependency_graph_reads_a_container_tree() {
        let _guard = env_lock();
        let (_state, id, home, runtime) = setup(false);
        let directory = container_directory(runtime.to_string_lossy().as_ref(), &id);
        let harness = directory.join("harness/packages");
        write_graph_package(
            &harness.join("core/subprocess"),
            "@deepseek-ai/dsh-subprocess",
            &[],
            "export class Subprocess extends Service {\n  constructor(ctx) { super(ctx, 'subprocess') }\n}\n",
        );
        write_graph_package(
            &harness.join("core/tools"),
            "@deepseek-ai/dsh-tools",
            &["@deepseek-ai/dsh-subprocess"],
            "export const inject = ['subprocess']\nexport class ToolRuntime extends Service {\n  constructor(ctx) { super(ctx, 'tools') }\n}\n",
        );
        // The profile activates the tools bundle; its own dependency pulls in
        // subprocess.
        let profile = directory.join("profile/profiles/web");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("package.json"),
            serde_json::json!({
                "name": "dsh-profile-web",
                "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-tools"] } }
            })
            .to_string(),
        )
        .unwrap();

        let graph = plugin_dependency_graph_rpc(&json!({ "kind": "container", "id": id })).unwrap();

        assert_eq!(graph["source"], "container");
        assert_eq!(graph["sourceId"], id);
        assert_eq!(graph["profile"], "web");
        assert_eq!(graph["services"], json!(["subprocess", "tools"]));
        // Dependencies are ordered before their dependents.
        assert_eq!(
            graph["order"],
            json!(["@deepseek-ai/dsh-subprocess", "@deepseek-ai/dsh-tools"])
        );
        assert!(graph["missing"].as_array().unwrap().is_empty());
        assert!(graph["cycles"].as_array().unwrap().is_empty());
        assert!(graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["from"] == "@deepseek-ai/dsh-tools"
                && link["to"] == "@deepseek-ai/dsh-subprocess"
                && link["service"] == "subprocess"));
        cleanup(&home, &runtime);
    }

    #[test]
    fn plugin_dependency_graph_reports_an_unmaterialised_tree() {
        // A container whose harness was never materialised still answers, with an
        // empty graph and a diagnostic, instead of failing the request.
        let _guard = env_lock();
        let (_state, id, home, runtime) = setup(false);
        let graph = plugin_dependency_graph_rpc(&json!({ "kind": "container", "id": id })).unwrap();
        assert!(graph["plugins"].as_array().unwrap().is_empty());
        assert!(!graph["diagnostics"].as_array().unwrap().is_empty());
        cleanup(&home, &runtime);
    }

    #[test]
    fn plugin_dependency_graph_rejects_bad_requests() {
        let _guard = env_lock();
        let (_state, id, home, runtime) = setup(false);
        let err = plugin_dependency_graph_rpc(&json!({ "kind": "container" })).unwrap_err();
        assert!(err.contains("requires an id"), "got: {err}");
        let err =
            plugin_dependency_graph_rpc(&json!({ "kind": "container", "id": "../etc/passwd" }))
                .unwrap_err();
        assert!(err.contains("invalid container id"), "got: {err}");
        let err = plugin_dependency_graph_rpc(&json!({ "kind": "sausage", "id": id })).unwrap_err();
        assert!(err.contains("unknown plugin graph source"), "got: {err}");
        cleanup(&home, &runtime);
    }

    #[test]
    fn plugin_dependency_graph_reports_an_unknown_template() {
        let _guard = env_lock();
        let (_state, _id, home, runtime) = setup(false);
        let err =
            plugin_dependency_graph_rpc(&json!({ "kind": "template", "id": "nope" })).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
        cleanup(&home, &runtime);
    }
