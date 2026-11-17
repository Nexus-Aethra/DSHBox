//! Settings RPCs: mirror, GitHub token, runtime directory, and the harness
//! mirror info/purge pair.
//!
//! A small module on purpose: settings change for reasons that have nothing
//! to do with containers or templates, so they change on their own schedule.

use super::*;

/// The shared Harness mirror as the CLI and the UI need to see it: whether it
/// exists, how big it is, and where it points.
///
/// Read-only and cheap (one directory walk), so it answers inline.
pub(super) fn harness_mirror_info_rpc() -> Result<HandlerResult, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mirror = box_runtime::mirror::mirror_path(std::path::Path::new(&root));
    let present = box_runtime::mirror::is_usable(&mirror);
    let size_bytes = box_runtime::mirror::mirror_size_bytes(&mirror);
    Ok(HandlerResult::Sync(json!({
        "present": present,
        "path": mirror.to_string_lossy(),
        "sizeBytes": size_bytes,
        // The mirror is a cache: reporting it as user data would make "purge"
        // look like deleting something the user owns.
        "cache": true,
    })))
}

pub(super) fn enqueue_purge_harness_mirror(state: &DaemonState) -> Result<HandlerResult, String> {
    enqueue_task_worker(
        state,
        "harness-mirror-purge",
        vec!["harness-mirror".to_owned()],
        json!({}),
        |task| {
            let root = read_config()?
                .runtime_directory
                .ok_or("DSH Box storage is not configured")?;
            let freed = box_runtime::mirror::purge(std::path::Path::new(&root))
                .map_err(|error| error.into_message())?;
            task.log(&format!(
                "removed the shared Harness mirror ({} MB freed); the next pull rebuilds it",
                freed / (1024 * 1024)
            ));
            Ok(())
        },
    )
    .map(HandlerResult::Async)
}


pub(super) fn save_mirror_settings_rpc(request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    config.github_mirror = optional_string(&request["githubMirror"]);
    config.npm_registry = optional_string(&request["npmRegistry"]);
    write_config(&config)?;
    Ok(json!({ "saved": true }))
}

/// Store or clear the GitHub API token. Trimmed here as well as in the
/// desktop command -- the RPC is reachable from other callers, and a padded
/// token authenticates as itself. An empty string clears it, so the settings
/// field is its own eraser.
pub(super) fn save_github_token_rpc(request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    config.github_token = request["githubToken"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    write_config(&config)?;
    Ok(json!({ "saved": true }))
}

pub(super) fn save_runtime_directory_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    let directory = optional_string(&request["runtimeDirectory"]);
    if directory.is_none() {
        return Err("runtime directory cannot be empty".to_owned());
    }
    config.runtime_directory = directory;
    write_config(&config)?;
    // Re-derive the in-memory box paths so the task queue and every later
    // worker resolve the newly chosen storage root without a restart (a
    // daemon started before onboarding must adopt the new directory).
    state.adopt_config(&config)?;
    // Onboarding path: the daemon startup copy deferred because no runtime
    // directory was configured yet, so vendor the bundled plugins now.
    if let Err(error) = crate::state::initialize_bundled_plugins() {
        return Err(format!("cannot vendor bundled plugins: {error}"));
    }
    Ok(json!({ "saved": true, "restartRequired": true }))
}
