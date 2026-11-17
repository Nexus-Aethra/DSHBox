//! tests for deletion_queue.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::io;

    fn fresh_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dshbox-queue-test-{}-{}",
            now_seconds(),
            uuid::Uuid::new_v4().simple().to_string(),
        ))
    }

    /// A remove function that always fails. Lets us exercise the
    /// "promote to slow" / "exhaust retries" code paths deterministically
    /// without depending on platform-specific `remove_dir_all` failures
    /// (Windows file locking is unreliable: an open `File` handle does not
    /// necessarily prevent `remove_dir_all` from succeeding).
    fn always_fail_remove(_path: &Path) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Other, "injected failure"))
    }

    /// Touch a real file at `path` so `path.exists()` returns true. The
    /// injected remove function never touches the filesystem, so the file
    /// is just a marker for the existence check; the test cleans up the
    /// whole root directory at the end.
    fn touch_marker(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"placeholder").unwrap();
    }

    #[test]
    fn enqueue_and_drain_fast() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target-dir");
        fs::create_dir_all(&target).unwrap();

        enqueue_for_hard_delete(&root, "plugin:abc", &target.to_string_lossy().to_string()).unwrap();
        assert_eq!(read_deletion_queue(&root).fast.len(), 1);

        let (succeeded, promoted) = drain_fast_queue(&root);
        assert_eq!(succeeded, 1);
        assert_eq!(promoted, 0);
        assert!(!target.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn drain_fast_promotes_failure_to_slow() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target-dir");
        touch_marker(&target);
        let target_str = target.to_string_lossy().to_string();

        // Pre-seed the fast queue directly so the test scenario is fully
        // independent from `enqueue_for_hard_delete` (which is exercised
        // by `enqueue_and_drain_fast`).
        let mut queue = read_deletion_queue(&root);
        queue.fast.push(DeletionQueueEntry {
            id: "plugin:abc".to_owned(),
            path: target_str.clone(),
            enqueued_at: now_seconds(),
            retry_count: 0,
            last_error: None,
        });
        write_deletion_queue(&root, &queue).unwrap();
        assert_eq!(read_deletion_queue(&root).fast.len(), 1);

        let (succeeded, promoted) = drain_fast_queue_with(&root, always_fail_remove);
        // remove failed → entry promoted to slow with retry_count = 1.
        assert_eq!(succeeded, 0);
        assert_eq!(promoted, 1);
        let after = read_deletion_queue(&root);
        assert!(after.fast.is_empty());
        assert_eq!(after.slow.len(), 1);
        assert_eq!(after.slow[0].id, "plugin:abc");
        assert_eq!(after.slow[0].path, target_str);
        assert_eq!(after.slow[0].retry_count, 1);
        assert!(after.slow[0].last_error.is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_path_is_noop_success() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        enqueue_for_hard_delete(&root, "plugin:gone", "/nonexistent/path").unwrap();

        let (succeeded, promoted) = drain_fast_queue(&root);
        assert_eq!(succeeded, 1);
        assert_eq!(promoted, 0);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn process_slow_exhausts_retries_to_permanent() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target-dir");
        touch_marker(&target);
        let target_str = target.to_string_lossy().to_string();

        // Pre-seed a slow entry at MAX_RETRIES. The path must exist so the
        // early-return for "already gone" does not fire and the remove
        // function is actually invoked.
        let mut queue = read_deletion_queue(&root);
        queue.slow.push(DeletionQueueEntry {
            id: "plugin:hard".to_owned(),
            path: target_str.clone(),
            enqueued_at: now_seconds(),
            retry_count: MAX_RETRIES,
            last_error: Some("prior error".to_owned()),
        });
        write_deletion_queue(&root, &queue).unwrap();

        let (succeeded, permanent, requeued) = process_slow_queue_with(&root, always_fail_remove);
        assert_eq!(succeeded, 0);
        assert_eq!(permanent, 1);
        assert_eq!(requeued, 0);
        let after = read_deletion_queue(&root);
        assert!(after.slow.is_empty());
        assert_eq!(after.permanent_failures.len(), 1);
        assert_eq!(after.permanent_failures[0].id, "plugin:hard");
        assert_eq!(after.permanent_failures[0].path, target_str);
        // The permanent failure records the *latest* error from the remove
        // call, not the entry's prior `last_error`.
        assert_eq!(after.permanent_failures[0].last_error, "injected failure");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn process_slow_retries_until_exhausted() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target-dir");
        touch_marker(&target);
        let target_str = target.to_string_lossy().to_string();

        // Pre-seed a slow entry at retry_count 1.
        let mut queue = read_deletion_queue(&root);
        queue.slow.push(DeletionQueueEntry {
            id: "plugin:retry".to_owned(),
            path: target_str,
            enqueued_at: now_seconds(),
            retry_count: 1,
            last_error: None,
        });
        write_deletion_queue(&root, &queue).unwrap();

        // First poll: 1 < MAX_RETRIES → requeued at 2 with last_error set.
        let (succeeded, permanent, requeued) = process_slow_queue_with(&root, always_fail_remove);
        assert_eq!(succeeded, 0);
        assert_eq!(permanent, 0);
        assert_eq!(requeued, 1);
        let after_first = read_deletion_queue(&root);
        assert_eq!(after_first.slow.len(), 1);
        assert_eq!(after_first.slow[0].id, "plugin:retry");
        assert_eq!(after_first.slow[0].retry_count, 2);
        assert!(after_first.slow[0].last_error.is_some());

        // Bump retry_count to MAX_RETRIES and poll again → permanent.
        {
            let mut q = read_deletion_queue(&root);
            q.slow[0].retry_count = MAX_RETRIES;
            write_deletion_queue(&root, &q).unwrap();
        }
        let (succeeded, permanent, requeued) = process_slow_queue_with(&root, always_fail_remove);
        assert_eq!(succeeded, 0);
        assert_eq!(permanent, 1);
        assert_eq!(requeued, 0);
        let after_second = read_deletion_queue(&root);
        assert!(after_second.slow.is_empty());
        assert_eq!(after_second.permanent_failures.len(), 1);
        assert_eq!(after_second.permanent_failures[0].id, "plugin:retry");

        let _ = fs::remove_dir_all(root);
    }
