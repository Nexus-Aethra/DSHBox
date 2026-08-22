//! Background `DeletionWorker` thread. Drains the fast queue on every
//! tick and polls the slow queue every `SLOW_INTERVAL` seconds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::queue::{drain_fast_queue, process_slow_queue, SLOW_INTERVAL_SECS};

/// A background thread that continuously processes the deletion queue.
///
/// Lifecycle:
/// 1. `DeletionWorker::new(runtime)` spawns the thread.
/// 2. On every 100 ms tick: drain the fast queue (immediate), then poll
///    the slow queue if `SLOW_INTERVAL` seconds have elapsed since the
///    last slow poll.
/// 3. `DeletionWorker::stop()` signals shutdown and joins the thread.
///
/// The worker reads and writes the queue JSON file on each iteration — it
/// is safe to call the `queue` module functions from other threads
/// concurrently, because each operation is read-modify-write with atomic
/// file replacement.
pub struct DeletionWorker {
    handle: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl DeletionWorker {
    pub fn new(runtime: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let runtime_clone = runtime.to_path_buf();

        let handle = thread::Builder::new()
            .name("dshbox-data-scheduler".to_owned())
            .spawn(move || {
                Self::thread_main(&runtime_clone, &stop_clone);
            })
            .expect("failed to spawn data scheduler worker");

        Self {
            handle: Some(handle),
            stop,
        }
    }

    /// Signal the worker to stop and wait for it to join. Idempotent —
    /// calling `stop` twice is a no-op on the second call.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Check whether the worker thread is still running.
    pub fn is_running(&self) -> bool {
        self.stop.load(Ordering::SeqCst) == false
    }

    fn thread_main(runtime: &Path, stop: &AtomicBool) {
        let tick = Duration::from_millis(100);
        let slow_interval = Duration::from_secs(SLOW_INTERVAL_SECS);

        // Cold start: drain any pending fast entries immediately.
        let _ = drain_fast_queue(runtime);

        let mut last_slow_poll = std::time::Instant::now();

        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }

            let _ = drain_fast_queue(runtime);

            if std::time::Instant::now() - last_slow_poll >= slow_interval {
                let _ = process_slow_queue(runtime);
                last_slow_poll = std::time::Instant::now();
            }

            thread::sleep(tick);
        }
    }
}

impl Drop for DeletionWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
