use super::*;
use super::commands;

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    is_safe_identifier(version)
}

pub(crate) fn fetch_dsh_tags() -> Result<Vec<String>, String> {
    let config = read_config()?;
    let endpoint = mirror_url(DSH_TAGS_API, config.github_mirror.as_deref());
    let client = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("cannot create GitHub client: {error}"))?;
    let response = client
        .get(&endpoint)
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let tags: Vec<GitHubTag> = response
        .error_for_status()
        .map_err(|error| format!("GitHub tags request failed: {error}"))?
        .json()
        .map_err(|error| format!("cannot parse GitHub tags: {error}"))?;
    Ok(tags
        .into_iter()
        .map(|tag| tag.name)
        .filter(|name| is_safe_version_name(name))
        .collect())
}

pub(crate) fn dsh_catalog_path(root: &str) -> PathBuf {
    PathBuf::from(root).join("state/dsh-catalog.json")
}

pub(crate) fn read_dsh_catalog(root: &str) -> Vec<String> {
    fs::read_to_string(dsh_catalog_path(root))
        .ok()
        .and_then(|source| serde_json::from_str::<Vec<String>>(&source).ok())
        .unwrap_or_default()
}

/// How long a fetched version catalog stays valid before the GitHub API is
/// called again. The versions page triggers a refresh on every visit, so
/// without this window every tab switch would hit GitHub and trip its rate
/// limits; tags change rarely, ten minutes is plenty fresh.
pub(crate) const DSH_CATALOG_TTL_SECONDS: u64 = 600;

pub(crate) fn refresh_dsh_catalog() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = dsh_catalog_path(&root);
    // Reuse a recent catalog instead of hitting the network again.
    if let Ok(metadata) = fs::metadata(&path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = modified.elapsed() {
                if age.as_secs() < DSH_CATALOG_TTL_SECONDS {
                    return Ok(());
                }
            }
        }
    }
    let tags = fetch_dsh_tags()?;
    fs::create_dir_all(path.parent().ok_or("invalid DSH catalog path")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string(&tags).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn enqueue_dsh_version_install(
    version: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "dsh-version-install",
        vec![format!("runtime:{version}")],
        serde_json::json!({ "version": version }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Cloning DSH source", 10);
        task.check_cancelled()?;
        task.log("starting DSH clone");
        let cancellation = task.clone();
        let result = commands::versions::install_dsh_version_with_cancel(version, move || {
            cancellation.cancelled()
        })
        .map(|_| ());
        task.update("Finalizing DSH runtime", 95);
        result
    });
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_dsh_catalog_refresh(
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "dsh-catalog-refresh",
        vec!["catalog:dsh".to_owned()],
        serde_json::json!({}),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Fetching DSH versions", 20);
        task.log("requesting DSH version catalog from GitHub");
        task.check_cancelled()?;
        refresh_dsh_catalog()?;
        task.check_cancelled()?;
        task.update("Version catalog refreshed", 95);
        Ok(())
    });
    Ok(task)
}
