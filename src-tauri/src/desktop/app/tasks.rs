use super::*;
use tauri::Emitter;

pub(crate) fn task_records(manager: &TaskManager) -> Vec<TaskRecord> {
    manager.list().unwrap_or_default()
}

/// Rebuilds the read model from files and live process ownership. Failures are
/// intentionally non-fatal because operations must not be blocked by diagnostics.
pub(crate) fn refresh_global_state(app: &tauri::AppHandle) {
    let Ok(config) = read_config() else {
        return;
    };
    let mut containers = config
        .runtime_directory
        .as_deref()
        .and_then(|root| scan_containers(root).ok())
        .map(|items| items.into_values().collect::<Vec<_>>())
        .unwrap_or_default();
    if let Ok(mut running) = app.state::<ContainerManager>().running.lock() {
        for container in &mut containers {
            container.status = match running.get_mut(&container.id) {
                Some(host) => match host.child.try_wait() {
                    Ok(None) => "running".to_owned(),
                    Ok(Some(_)) | Err(_) => "stopped".to_owned(),
                },
                None => "stopped".to_owned(),
            };
        }
    }
    let versions = config
        .runtime_directory
        .as_deref()
        .and_then(|root| installed_dsh_versions(root).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|name| DshVersion {
            name,
            installed: true,
        })
        .collect();
    let state = app.state::<ResourceStateManager>();
    state.refresh_all(&config, scan_toolchains(&config), versions, containers);
    state.replace_tasks(task_records(&app.state::<TaskManager>()));
    if let Ok(paths) = BoxPaths::from_config(&config) {
        let _ = state.write_snapshot(&paths);
    }
}

pub(crate) fn task_paths() -> Result<BoxPaths, String> {
    let config = read_config()?;
    BoxPaths::from_config(&config)
}

pub(crate) fn persist_tasks(manager: &TaskManager) -> Result<(), String> {
    manager.persist(&task_paths()?)
}

pub(crate) fn queue_task(
    manager: &TaskManager,
    app: &tauri::AppHandle,
    kind: &str,
    resource_keys: Vec<String>,
    params: serde_json::Value,
) -> Result<TaskRecord, String> {
    let task = manager.enqueue(&task_paths()?, kind, resource_keys, params)?;
    let state = app.state::<ResourceStateManager>();
    state.apply_task_update(task.clone());
    if let Ok(config) = read_config() {
        if let Ok(paths) = BoxPaths::from_config(&config) {
            let _ = state.write_snapshot(&paths);
        }
    }
    app.emit("task://created", &task)
        .map_err(|error| error.to_string())?;
    Ok(task)
}

pub(crate) fn append_task_log(manager: &TaskManager, app: &tauri::AppHandle, task_id: &str, message: &str) {
    let task = manager.task(task_id).ok();
    if let Some(task) = task {
        let line = format!("[{}] {message}\n", now_seconds());
        let _ = fs::OpenOptions::new()
            .append(true)
            .open(&task.log_path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        let _ = app.emit(
            "task://log",
            serde_json::json!({ "taskId": task_id, "line": line.trim_end() }),
        );
    }
}

pub(crate) fn emit_task_update(manager: &TaskManager, app: &tauri::AppHandle, task_id: &str) {
    if let Ok(task) = manager.task(task_id) {
        let _ = app.emit("task://updated", task);
    }
    let _ = persist_tasks(manager);
    let state = app.state::<ResourceStateManager>();
    if let Ok(task) = manager.task(task_id) {
        state.apply_task_update(task);
    }
    if let Ok(config) = read_config() {
        if let Ok(paths) = BoxPaths::from_config(&config) {
            let _ = state.write_snapshot(&paths);
        }
    }
}

pub(crate) fn ensure_resource_idle(manager: &TaskManager, resource: &str) -> Result<(), String> {
    if !manager.resource_idle(resource)? {
        Err(format!("resource is busy: {resource}"))
    } else {
        Ok(())
    }
}

/// Host adapter that forwards scheduler notifications to the tauri event bus
/// and the persisted resource snapshot, so the scheduler crate itself stays
/// framework-free.
struct TauriNotifier {
    manager: TaskManager,
    app: tauri::AppHandle,
}

impl TaskNotifier for TauriNotifier {
    fn stage(&self, task_id: &str, _stage: &str, _progress: u8) {
        emit_task_update(&self.manager, &self.app, task_id);
    }

    fn log(&self, task_id: &str, line: &str) {
        append_task_log(&self.manager, &self.app, task_id, line);
    }
}

pub(crate) fn run_queued_task(
    manager: TaskManager,
    app: tauri::AppHandle,
    task_id: String,
    work: impl FnOnce(&TaskContext) -> Result<(), String> + Send + 'static,
) {
    thread::spawn(move || {
        let Ok(paths) = task_paths() else {
            return;
        };
        let notifier = TauriNotifier {
            manager: manager.clone(),
            app: app.clone(),
        };
        run_queued(&manager, &paths, std::sync::Arc::new(notifier), &task_id, work);
        if let Ok(task) = manager.task(&task_id) {
            let _ = app.emit("task://finished", task);
        }
        refresh_global_state(&app);
    });
}

#[tauri::command]
pub(crate) fn list_tasks(resources: tauri::State<ResourceStateManager>) -> Result<Vec<TaskRecord>, String> {
    Ok(resources.snapshot()?.tasks)
}

#[tauri::command]
pub(crate) fn cancel_task(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    manager.request_cancel(&task_paths()?, &id)?;
    emit_task_update(&manager, &app, &id);
    Ok(())
}

#[tauri::command]
pub(crate) fn delete_task(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    manager.remove(&task_paths()?, &id)?;
    refresh_global_state(&app);
    Ok(())
}

#[tauri::command]
pub(crate) fn read_task_log(
    id: String,
    resources: tauri::State<ResourceStateManager>,
) -> Result<String, String> {
    let task = resources
        .snapshot()?
        .tasks
        .into_iter()
        .find(|task| task.id == id)
        .ok_or("task not found")?;
    fs::read_to_string(task.log_path).map_err(|error| format!("cannot read task log: {error}"))
}

#[tauri::command]
pub(crate) fn retry_task(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let previous = manager.task(&id)?;
    match previous.kind.as_str() {
        "toolchain-install" => enqueue_toolchain_install(
            previous.params["id"]
                .as_str()
                .ok_or("task has no toolchain id")?
                .to_owned(),
            manager,
            app,
        ),
        "dsh-version-install" => enqueue_dsh_version_install(
            previous.params["version"]
                .as_str()
                .ok_or("task has no DSH version")?
                .to_owned(),
            manager,
            app,
        ),
        "dsh-catalog-refresh" => enqueue_dsh_catalog_refresh(manager, app),
        "container-start" => enqueue_container_start(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-stop" => enqueue_container_stop(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-rebuild" => enqueue_container_rebuild(
            previous.params["id"]
                .as_str()
                .ok_or("task has no container id")?
                .to_owned(),
            manager,
            app,
        ),
        "container-extension-add" => enqueue_container_extension_add(
            AddContainerExtensionRequest {
                id: previous.params["id"]
                    .as_str()
                    .ok_or("task has no container id")?
                    .to_owned(),
                profile: previous.params["profile"]
                    .as_str()
                    .ok_or("task has no profile")?
                    .to_owned(),
                source: previous.params["source"]
                    .as_str()
                    .ok_or("task has no extension source")?
                    .to_owned(),
            },
            manager,
            app,
        ),
        "repository-extension-import" => enqueue_repository_extension_import(
            ImportRepositoryExtensionRequest { source: previous.params["source"].as_str().ok_or("task has no extension source")?.to_owned() }, manager, app,
        ),
        "workspace-extension-import" => enqueue_workspace_extension_import(
            ImportWorkspaceExtensionRequest {
                id: previous.params["id"].as_str().ok_or("task has no container id")?.to_owned(),
                relative_path: previous.params["relativePath"].as_str().ok_or("task has no workspace path")?.to_owned(),
            }, manager, app,
        ),
        "container-extension-copy" => enqueue_container_extension_copy(
            CopyRepositoryExtensionRequest {
                id: previous.params["id"].as_str().ok_or("task has no container id")?.to_owned(),
                profile: previous.params["profile"].as_str().map(str::to_owned),
                repository_id: previous.params["repositoryId"].as_str().ok_or("task has no repository extension")?.to_owned(),
            }, manager, app,
        ),
        "repository-extension-export" => enqueue_repository_extension_export(
            ExportRepositoryExtensionRequest {
                repository_id: previous.params["repositoryId"].as_str().ok_or("task has no repository extension")?.to_owned(),
                destination: previous.params["destination"].as_str().ok_or("task has no export destination")?.to_owned(),
            }, manager, app,
        ),
        _ => Err("this task type cannot be retried".to_owned()),
    }
}
