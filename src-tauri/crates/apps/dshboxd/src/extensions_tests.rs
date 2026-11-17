//! tests for extensions.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use std::env;

    struct NoopNotifier;
    impl box_scheduler::TaskNotifier for NoopNotifier {
        fn stage(&self, _task_id: &str, _stage: &str, _progress: u8) {}
        fn log(&self, _task_id: &str, _line: &str) {}
    }

    fn sandbox(name: &str) -> PathBuf {
        // A UUID, not now_seconds(): a wall-clock second is coarse enough
        // that a re-run inside the same second reuses the previous
        // directory, and this root is a document store another test may
        // still be reading.
        let dir = env::temp_dir().join(format!(
            "dshboxd-ext-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(home: &Path, runtime: &Path) {
        let config_dir = home.join(".dsh-box");
        fs::create_dir_all(&config_dir).unwrap();
        let body = serde_json::json!({ "runtimeDirectory": runtime.to_string_lossy() });
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    fn test_task(runtime: &Path) -> TaskContext {
        let paths = BoxPaths {
            config: runtime.join("config.json"),
            runtime: Some(runtime.to_path_buf()),
        };
        let manager = TaskManager::memory();
        let record = manager
            .enqueue(&paths, "test", Vec::new(), serde_json::json!({}))
            .unwrap();
        TaskContext {
            manager,
            paths,
            notifier: std::sync::Arc::new(NoopNotifier),
            task_id: record.id,
            profile_dir: None,
        }
    }

    fn make_plugin_source(name: &str, version: &str) -> PathBuf {
        // Mix nanosecond timestamp with PID so back-to-back invocations
        // never collide on the same directory name. Seconds-resolution
        // timestamps were prone to races when cargo test ran two cases
        // within the same wall-clock second.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = env::temp_dir().join(format!(
            "dshboxd-ext-src-{name}-{version}-{pid}-{nanos}"
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({
            "name": name,
            "version": version,
            "description": "test plugin",
            "dsh": {
                "bundle": {
                    "patch": "[]",
                },
            },
        });
        fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        dir
    }

    // Importing the same plugin twice should reuse the entry id from the
    // first call rather than minting a second `img-<task_id>` row.
    #[test]
    fn import_dedup_by_name_and_version() {
        // Acquire the daemon-wide test lock so we don't race with other
        // tests that mutate HOME (e.g. dispatch::tests::describe_container_*);
        // otherwise the config file is sometimes gone before the daemon's
        // `read_config` looks at it.
        let _lock = crate::test_support::env_lock();
        let home = sandbox("dedup-home");
        let runtime = sandbox("dedup-runtime");
        write_config(&home, &runtime);
        // `box_foundation::config_path` honours `DSHBOX_CONFIG_DIR`; this
        // is what production-like reads see on Windows where `HOME` is
        // ignored by `dirs::home_dir()`. The HOME line is kept for
        // compatibility with Linux/macOS where `dirs` falls back to it.
        let _guard = EnvGuard::set("HOME", &home);
        let _dshbox_dir = EnvGuard::set("DSHBOX_CONFIG_DIR", &home.join(".dsh-box"));
        // Repository imports now produce an immutable `npm pack` artifact.
        // Production initializes this once during daemon startup; unit tests
        // exercise the same path explicitly (another concurrent test may
        // have initialized it already, which is equally valid).
        let _ = crate::state::initialize_bundled_runtime();

        let source_a = make_plugin_source("dsh-better-sidebar", "0.12.3");
        let task = test_task(&runtime);
        let first = import_into_repository(&task, &source_a).unwrap();
        let source_b = make_plugin_source("dsh-better-sidebar", "0.12.3");
        let second = import_into_repository(&task, &source_b).unwrap();
        assert_eq!(
            first.id, second.id,
            "second import should reuse the cached entry"
        );
        let entries = scan_repository(&runtime);
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.name == "dsh-better-sidebar" && entry.version.as_deref() == Some("0.12.3")
            })
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "exactly one entry should match the package identity"
        );

        // Different version skews the cache key.
        let source_c = make_plugin_source("dsh-better-sidebar", "0.12.4");
        let task_v2 = test_task(&runtime);
        let third = import_into_repository(&task_v2, &source_c).unwrap();
        assert_ne!(
            first.id, third.id,
            "different version must create a new entry"
        );
        let entries = scan_repository(&runtime);
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| entry.name == "dsh-better-sidebar")
            .collect();
        assert_eq!(matching.len(), 2, "two distinct versions should coexist");

        let _ = fs::remove_dir_all(&source_a);
        let _ = fs::remove_dir_all(&source_b);
        let _ = fs::remove_dir_all(&source_c);
        // The two sandbox roots themselves: they hold a document store and a
        // HOME, so leaving them behind means the next run adopts this one's rows.
        let _ = fs::remove_dir_all(&runtime);
        let _ = fs::remove_dir_all(&home);
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests in this module run on a single thread by default
            // (no other tests touch HOME concurrently), and we restore the
            // previous value on drop.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { env::set_var(self.key, value) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn plugin_artifact_lives_next_to_the_repository_source() {
        let entry = RepositoryExtension {
            id: "img-1".to_owned(),
            kind: ExtensionKind::Plugin,
            name: "@example/plugin".to_owned(),
            version: Some("1.0.0".to_owned()),
            description: None,
            content_digest: "digest".to_owned(),
            source_path: "D:/runtime/repository/plugins/img-1/source".to_owned(),
            imported_at: 0,
            diagnostic: None,
            source: None,
            storage: box_extensions::RepositoryStorage::Owned,
            derived: false,
        };
        assert_eq!(
            repository_plugin_artifact(&entry).unwrap(),
            PathBuf::from("D:/runtime/repository/plugins/img-1/artifact.tgz")
        );
    }
