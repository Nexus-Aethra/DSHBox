//! Test-only helpers shared across the daemon crate.
//!
//! Currently exposes [`env_lock`], a process-global `Mutex` used to
//! serialise every test that mutates `HOME` (or the daemon's config
//! dir) and the persisted config files underneath. Without this lock,
//! `cargo test` would run tests in parallel and one test's `cleanup`
//! would race another's `read_config`, surfacing as flaky
//! `DSH Box storage is not configured` failures.

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquire the global test lock. Hold the returned guard for the entire
/// lifetime of the test (including any `cleanup` step that removes the
/// sandboxed home directory) — releasing it early defeats the point.
#[cfg(test)]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|guard| guard.into_inner())
}
