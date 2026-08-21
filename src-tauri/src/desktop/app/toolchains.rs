use super::*;

pub(crate) fn detect_toolchain(id: &str, name: &str) -> ToolchainStatus {
    let version = bundled_runtime().ok().and_then(|runtime| match id {
        "node" => Some(runtime.node_version.clone()),
        "npm" => Some(runtime.npm_version.clone()),
        "pnpm" => Some(runtime.pnpm_version.clone()),
        _ => None,
    });
    ToolchainStatus {
        id: id.to_owned(),
        name: name.to_owned(),
        system_version: None,
        managed_version: version,
    }
}

pub(crate) fn scan_toolchains(_: &BoxConfig) -> Vec<ToolchainStatus> {
    [
        detect_toolchain("node", "Node.js"),
        detect_toolchain("npm", "npm"),
        detect_toolchain("pnpm", "pnpm"),
    ]
    .into()
}

#[tauri::command]
pub(crate) fn start_toolchain_install(id: String) -> Result<ToolchainInstallStatus, String> {
    if !is_known_toolchain(&id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    Err("Node, npm, and pnpm are bundled with DSH Box; reinstall the application to repair the runtime".to_owned())
}

#[tauri::command]
pub(crate) fn enqueue_toolchain_install(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "toolchain-install",
        vec![format!("toolchain:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Preparing toolchain installation", 5);
        task.check_cancelled()?;
        task.log("starting toolchain installer");
        let result = start_toolchain_install(id).map(|_| ());
        task.update("Finalizing toolchain installation", 95);
        result
    });
    Ok(task)
}
