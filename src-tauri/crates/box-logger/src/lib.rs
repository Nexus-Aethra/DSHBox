//! Unified tracing logger for every DSH Box crate.
//!
//! Each binary (daemon / desktop / CLI) calls [`init`] once at startup with
//! a [`LogComponent`] tag. `init` configures a global `tracing_subscriber`
//! with two output layers:
//!
//! 1. **File layer** — a daily rolling appender writing to
//!    `<runtime>/logs/<component>.log`. Keeps a full structured history
//!    on disk for offline debugging without leaking to a terminal that
//!    may not exist (tray autostart, Windows service).
//! 2. **stderr layer** — human-friendly fmt for live operator feedback.
//!
//! Both layers honour `RUST_LOG`. The default filter is
//! `info,box_template_core=debug,box_dsh_versions=debug,dshboxd=debug`
//! so the noisiest crates run at `debug` while everything else stays at
//! `info`.
//!
//! `init` returns a [`WorkerGuard`] from `tracing_appender` that the caller
//! **must** keep alive for the lifetime of the process — dropping it flushes
//! the writer thread and subsequent log lines would be silently lost.

use std::sync::OnceLock;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};

/// Which subsystem is initialising the logger. Drives the on-disk filename
/// (`<runtime>/logs/<component>.log`) and any per-component filter overrides
/// the user passes via `RUST_LOG`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogComponent {
    /// `dshboxd` background server.
    Daemon,
    /// `dshbox` desktop Tauri shell.
    Desktop,
    /// `dshbox` CLI subprocesses.
    Cli,
    /// Bundled-runtime installer / prober (`box-runtime-packager`).
    Bundled,
}

impl LogComponent {
    /// Filename stem (without `.log` extension). Used as the rolling-file
    /// prefix so each subsystem has its own log file.
    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Desktop => "desktop",
            Self::Cli => "cli",
            Self::Bundled => "bundled",
        }
    }
}

/// Process-global subscriber guard. Returned by [`init`] so the caller can
/// hold the `WorkerGuard` until shutdown. Also prevents double-init.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Initialise the global tracing subscriber. Idempotent — calling it more
/// than once returns the cached `WorkerGuard` without re-installing the
/// subscriber.
///
/// `log_dir` is the directory the rolling file appender writes into. For
/// daemon/desktop this should be `<runtime>/logs/<component>/`; for the CLI
/// it can fall back to `~/.dsh-box/logs/<component>/` when no runtime is
/// configured.
///
/// The returned `WorkerGuard` must be kept alive for the lifetime of the
/// process. Dropping it flushes the writer thread; log lines emitted after
/// the drop are silently lost.
pub fn init(component: LogComponent, log_dir: &std::path::Path) -> Result<&'static WorkerGuard, String> {
    if let Some(existing) = GUARD.get() {
        return Ok(existing);
    }
    std::fs::create_dir_all(log_dir)
        .map_err(|error| format!("cannot create log dir {}: {error}", log_dir.display()))?;
    let file_appender =
        tracing_appender::rolling::daily(log_dir, format!("{}.log", component.file_stem()));
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,box_template_core=debug,box_dsh_versions=debug,dshboxd=debug")
    });

    let registry = Registry::default()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(true),
        )
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(true),
        );

    registry
        .try_init()
        .map_err(|error| format!("cannot install tracing subscriber: {error}"))?;

    // OnceLock::set can race between threads; if two threads both win, the
    // second registration succeeds at the tracing layer and we end up with
    // duplicate subscribers. That's noisy but recoverable — log lines still
    // go to the same writer. We accept the race rather than serialise init.
    let _ = GUARD.set(file_guard);
    Ok(GUARD.get().expect("just installed"))
}

/// Resolve the per-component log directory.
///
/// Prefers `<runtime>/logs/<component>/` when `runtime` is configured; falls
/// back to `~/.dsh-box/logs/<component>/` so the CLI / prober can still
/// write logs before the user has pointed at a runtime.
pub fn log_dir(component: LogComponent, runtime: Option<&str>) -> std::path::PathBuf {
    if let Some(root) = runtime {
        return std::path::PathBuf::from(root)
            .join("logs")
            .join(component.file_stem());
    }
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    home.join(".dsh-box")
        .join("logs")
        .join(component.file_stem())
}