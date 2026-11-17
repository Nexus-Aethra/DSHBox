//! Dual-queue async hard-delete. Fast queue drains on every map write;
//! slow queue is polled every 60 s. Exhausted retries become permanent
//! failures.

use box_foundation::{now_seconds, BoxResult};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const MAX_RETRIES: u32 = 5;
pub(crate) const SLOW_INTERVAL_SECS: u64 = 60;

/// Runtime-relative path of the deletion queue.
pub fn deletion_queue_path(runtime: &Path) -> PathBuf {
    runtime.join("state").join("deletion-queue.json")
}

/// One item in the deletion queue.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionQueueEntry {
    pub id: String,
    pub path: String,
    pub enqueued_at: u64,
    pub retry_count: u32,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// A hard-delete that exhausted its retry budget.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermanentFailure {
    pub id: String,
    pub path: String,
    pub last_error: String,
    pub failed_at: u64,
}

/// The persisted deletion queue state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionQueue {
    pub fast: Vec<DeletionQueueEntry>,
    pub slow: Vec<DeletionQueueEntry>,
    #[serde(default)]
    pub permanent_failures: Vec<PermanentFailure>,
    #[serde(default)]
    pub last_processed_at: u64,
}

impl DeletionQueue {
    pub fn is_empty(&self) -> bool {
        self.fast.is_empty() && self.slow.is_empty()
    }
}

/// Read the deletion queue from disk.
pub fn read_deletion_queue(runtime: &Path) -> DeletionQueue {
    let path = deletion_queue_path(runtime);
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the deletion queue via atomic replace.
pub fn write_deletion_queue(runtime: &Path, queue: &DeletionQueue) -> BoxResult<()> {
    let path = deletion_queue_path(runtime);
    write_queue_atomic(&path, queue)
}

/// Enqueue a path for immediate (fast) hard deletion.
///
/// `id` and `path` come from `remove_resource` (the map already removed the
/// entry). The worker drains the fast queue on the next tick.
pub fn enqueue_for_hard_delete(runtime: &Path, id: &str, path: &str) -> BoxResult<()> {
    let mut queue = read_deletion_queue(runtime);
    let entry = DeletionQueueEntry {
        id: id.to_owned(),
        path: path.to_owned(),
        enqueued_at: now_seconds(),
        retry_count: 0,
        last_error: None,
    };
    queue.fast.push(entry);
    queue.last_processed_at = now_seconds();
    write_deletion_queue(runtime, &queue)
}

/// Default remove implementation used by the public `*_queue` entry points.
fn remove_dir_all_default(path: &Path) -> std::io::Result<()> {
    fs::remove_dir_all(path)
}

/// Drain the fast queue. Each entry is attempted once:
/// - If the path is already gone → success (no-op).
/// - If `remove` succeeds → success.
/// - If it fails → the entry is promoted to the slow queue with `retry_count = 1`.
///
/// Returns `(succeeded, promoted_to_slow)` counts.
pub fn drain_fast_queue(runtime: &Path) -> (u32, u32) {
    drain_fast_queue_with(runtime, remove_dir_all_default)
}

/// Drain the fast queue with a caller-supplied remove function. Tests use
/// this to exercise failure paths deterministically — Windows file locking
/// is unreliable, so we never try to make the real `fs::remove_dir_all`
/// fail in tests.
pub fn drain_fast_queue_with<F>(runtime: &Path, remove: F) -> (u32, u32)
where
    F: Fn(&Path) -> std::io::Result<()>,
{
    let mut queue = read_deletion_queue(runtime);
    let mut succeeded = 0u32;
    let mut promoted = 0u32;

    let fast_items: Vec<DeletionQueueEntry> = queue.fast.drain(..).collect();
    for mut entry in fast_items {
        let path = Path::new(&entry.path);
        if !path.exists() {
            succeeded += 1;
            continue;
        }
        match remove(path) {
            Ok(()) => succeeded += 1,
            Err(error) => {
                entry.retry_count = 1;
                entry.last_error = Some(error.to_string());
                queue.slow.push(entry);
                promoted += 1;
            }
        }
    }

    queue.last_processed_at = now_seconds();
    let _ = write_deletion_queue(runtime, &queue);
    (succeeded, promoted)
}

/// Process the slow queue. One entry at a time (no batch). If an entry
/// exceeds `MAX_RETRIES` it is moved to `permanent_failures` and a
/// diagnostic log is written. Otherwise the entry is re-enqueued with
/// `retry_count + 1`.
///
/// Returns `(succeeded, permanent_failures_added, requeued)` counts.
pub fn process_slow_queue(runtime: &Path) -> (u32, u32, u32) {
    process_slow_queue_with(runtime, remove_dir_all_default)
}

/// Process the slow queue with a caller-supplied remove function. See
/// [`drain_fast_queue_with`] for rationale.
pub fn process_slow_queue_with<F>(runtime: &Path, remove: F) -> (u32, u32, u32)
where
    F: Fn(&Path) -> std::io::Result<()>,
{
    let mut queue = read_deletion_queue(runtime);
    let mut succeeded = 0u32;
    let mut permanent = 0u32;
    let mut requeued = 0u32;

    let slow_items: Vec<DeletionQueueEntry> = queue.slow.drain(..).collect();
    for entry in slow_items {
        let path = Path::new(&entry.path);
        if !path.exists() {
            succeeded += 1;
            continue;
        }
        match remove(path) {
            Ok(()) => succeeded += 1,
            Err(error) => {
                if entry.retry_count >= MAX_RETRIES {
                    queue.permanent_failures.push(PermanentFailure {
                        id: entry.id.clone(),
                        path: entry.path.clone(),
                        last_error: error.to_string(),
                        failed_at: now_seconds(),
                    });
                    let _ = write_diagnostic_log(runtime, &entry.id, &entry.path, &error.to_string());
                    permanent += 1;
                } else {
                    let mut next = entry;
                    next.retry_count += 1;
                    next.last_error = Some(error.to_string());
                    queue.slow.push(next);
                    requeued += 1;
                }
            }
        }
    }

    queue.last_processed_at = now_seconds();
    let _ = write_deletion_queue(runtime, &queue);
    (succeeded, permanent, requeued)
}

/// Return the list of permanent failures (entries that could not be hard-deleted
/// after `MAX_RETRIES` attempts). These remain on disk; the resource map keeps
/// no record of them (they were already removed).
pub fn permanent_failures(runtime: &Path) -> Vec<PermanentFailure> {
    read_deletion_queue(runtime).permanent_failures
}

/// Clear the permanent failures log (e.g. after user intervention).
pub fn clear_permanent_failures(runtime: &Path) -> BoxResult<()> {
    let mut queue = read_deletion_queue(runtime);
    queue.permanent_failures.clear();
    write_deletion_queue(runtime, &queue)
}

/// Write a diagnostic log line for a permanent failure.
fn write_diagnostic_log(runtime: &Path, id: &str, path: &str, error: &str) -> BoxResult<()> {
    let log_dir = runtime.join("state").join("diag");
    fs::create_dir_all(&log_dir).map_err(|error| error.to_string())?;
    let log_path = log_dir.join(format!("hard-delete-failure-{}.log", id));
    let existing = fs::read_to_string(&log_path).unwrap_or_default();
    let line = format!(
        "[{}] permanent failure: id={} path={} error={}\n",
        now_seconds(),
        id,
        path,
        error,
    );
    fs::write(&log_path, existing + &line).map_err(|error| error.to_string())
}

/// Atomic JSON write via temp-file-then-rename. The tmp file sits next to
/// the target so `rename` is same-directory (always atomic on POSIX and
/// Windows NTFS).
pub(crate) fn write_queue_atomic<T: Serialize>(path: &Path, value: &T) -> BoxResult<()> {
    let parent = path.parent().ok_or("target path has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|error| error.to_string())?;
    let serialized = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp_name = format!("{}.json.tmp", stem);
    let tmp = parent.join(&tmp_name);
    fs::write(&tmp, serialized)
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    fs::rename(&tmp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
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
}
