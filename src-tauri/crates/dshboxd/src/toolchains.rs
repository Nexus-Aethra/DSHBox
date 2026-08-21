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
use std::{fs, path::Path};

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
pub(crate) fn pnpm_policy(
    toolchain: &ResolvedToolchain,
) -> Result<process::EnvironmentPolicy, String> {
    let config = read_config()?;
    let runtime_directory = config
        .runtime_directory
        .as_deref()
        .map(Path::new)
        .ok_or("DSH Box storage is not configured")?;
    let node_dir = std::path::Path::new(&toolchain.path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    // The pnpm tree hangs directly off the runtime root on every platform
    // (<root>/pnpm), next to the node/ tree. Derive it from the manifest
    // root rather than counting .parent() levels off the node binary — the
    // nesting depth differs (Windows ships node/node.exe, Unix
    // node/bin/node).
    let pnpm_dir = bundled_runtime()?.root.join("pnpm");
    let install_dir = dshbox_install_directory().ok();
    let bundled_git = bundled_runtime()?.git_dir.as_deref();
    // Linux always runs against the host's git (the developer-supplied
    // build at BoxConfig.git_path, then PATH, then well-known FHS bins).
    // Windows sticks with the bundled binary because Windows users do
    // not install git by default; failing loudly there is intentional.
    let host_git_dir = if box_runtime::bundled::target_is_linux() {
        box_runtime::bundled::resolve_host_git_dir(
            config.git_path.as_deref().map(Path::new),
        )
    } else {
        None
    };
    process::bundled_package_manager_policy(
        install_dir.as_deref(),
        &node_dir,
        &pnpm_dir,
        runtime_directory,
        config.npm_registry.as_deref(),
        bundled_git,
        host_git_dir.as_deref(),
    )
}

/// Add an actionable explanation when pnpm failed after repeatedly losing its
/// connection to a configured third-party registry. Optional native packages
/// may be skipped by pnpm after these failures, so the final lifecycle error
/// (for example a missing esbuild binary) is often misleading on its own.
pub(crate) fn pnpm_network_failure_hint(log_path: &Path) -> Option<String> {
    let config = read_config().ok()?;
    let registry = config.npm_registry?.trim().to_owned();
    if registry.is_empty() || is_official_npm_registry(&registry) {
        return None;
    }
    let log = fs::read_to_string(log_path).ok()?;
    let network_errors = [
        "ECONNRESET",
        "ETIMEDOUT",
        "UND_ERR_SOCKET",
        "error (23)",
        "ENOTFOUND",
        "ECONNREFUSED",
    ];
    let count = network_errors
        .iter()
        .map(|marker| log.matches(marker).count())
        .sum::<usize>();
    (count >= 2).then(|| {
        " The configured npm mirror had repeated network failures while downloading packages. \
         Switch the npm registry to the official registry or configure a Box-managed proxy, then retry; \
         pnpm will reuse packages already cached in this storage directory."
            .to_owned()
    })
}

fn is_official_npm_registry(registry: &str) -> bool {
    let normalized = registry.trim().trim_end_matches('/').to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "https://registry.npmjs.org" | "http://registry.npmjs.org"
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

#[cfg(test)]
mod tests {
    use super::is_official_npm_registry;

    #[test]
    fn recognizes_the_official_npm_registry() {
        assert!(is_official_npm_registry("https://registry.npmjs.org/"));
        assert!(is_official_npm_registry("http://registry.npmjs.org"));
        assert!(!is_official_npm_registry(
            "https://repo.huaweicloud.com/repository/npm/"
        ));
    }
}
