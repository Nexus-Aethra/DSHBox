//! Toolchain resolution for daemon-run tasks. Mirrors the desktop's
//! `toolchains.rs` but resolves against the daemon-owned bundled runtime.
//!
//! Also provides the shared `pnpm_policy`, `run_logged`, and `TaskCancel`
//! helpers that all daemon modules use for unified process execution.

use crate::state::{bundled_runtime, dshbox_install_directory};
use box_foundation::read_config;
use box_runtime::process::{
    self, runner::CancellationToken, ExecutionResult, LoggedProcess, NativeProcessRunner,
    ProcessSpec,
};
use box_scheduler::TaskContext;
use box_toolchains::is_known_toolchain;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedToolchain {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) arguments: Vec<String>,
}

pub(crate) fn resolve_toolchain(id: &str) -> Result<ResolvedToolchain, String> {
    if !is_known_toolchain(id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    let runtime = bundled_runtime()?;
    let (path, arguments) = match id {
        "node" => (runtime.node.clone(), Vec::new()),
        "npm" => (
            runtime.node.clone(),
            vec![runtime.npm.to_string_lossy().into_owned()],
        ),
        "pnpm" => (
            runtime.node.clone(),
            vec![runtime.pnpm.to_string_lossy().into_owned()],
        ),
        _ => return Err(format!("unsupported bundled toolchain: {id}")),
    };
    Ok(ResolvedToolchain {
        id: id.to_owned(),
        source: "bundled".to_owned(),
        path: path.to_string_lossy().into_owned(),
        arguments,
    })
}

/// Build a `bundled_toolchain_policy` for pnpm/npm invocations spawned by
/// the daemon. Mirrors the desktop's `command_for_toolchain` env setup
/// but goes through the unified module so daemon children get the same
/// registry/pnpm store pinning as the desktop.
pub(crate) fn pnpm_policy(toolchain: &ResolvedToolchain) -> process::EnvironmentPolicy {
    let config = read_config().ok();
    let node_dir = std::path::Path::new(&toolchain.path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let pnpm_dir = node_dir
        .parent()
        .map(|root| root.join("pnpm"))
        .unwrap_or_default();
    let install_dir = dshbox_install_directory().ok();
    process::bundled_toolchain_policy(
        install_dir.as_deref(),
        &node_dir,
        &pnpm_dir,
        config
            .as_ref()
            .and_then(|c| c.runtime_directory.as_deref())
            .map(Path::new),
        config.as_ref().and_then(|c| c.npm_registry.as_deref()),
        false,
    )
}

/// Wrap a `TaskContext` (or lack thereof) so it can be passed to the
/// unified process runner's `wait_or_kill` cancellation check.
pub(crate) struct TaskCancel<'a>(pub(crate) Option<&'a TaskContext>);

impl CancellationToken for TaskCancel<'_> {
    fn cancelled(&self) -> bool {
        self.0.map(TaskContext::cancelled).unwrap_or(false)
    }
}

/// Helper that wraps `NativeProcessRunner::execute` and only accepts the
/// logged variant, returning the inner `LoggedProcess`. The DSH host and
/// all pnpm/npm invocations funnel through this so error reporting is
/// uniform.
pub(crate) fn run_logged(spec: &ProcessSpec, description: &str) -> Result<LoggedProcess, String> {
    match NativeProcessRunner
        .execute(spec)
        .map_err(|error| format!("cannot start {description}: {error}"))?
    {
        ExecutionResult::Logged(logged) => Ok(logged),
        _ => Err(format!(
            "internal: expected logged execution for {description}"
        )),
    }
}
