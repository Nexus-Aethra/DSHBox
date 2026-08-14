use super::super::*;

#[tauri::command]
pub(crate) fn list_dsh_versions(
    resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<DshVersion>, String> {
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let installed_versions = installed_dsh_versions(&root)?;
    let mut versions = vec![DshVersion {
        name: "latest".to_owned(),
        installed: installed_versions.contains(&"latest".to_owned()),
    }];
    versions.extend(read_dsh_catalog(&root).into_iter().map(|name| DshVersion {
        installed: installed_versions.contains(&name),
        name,
    }));
    for name in installed_versions {
        if !versions.iter().any(|version| version.name == name) {
            versions.push(DshVersion {
                name,
                installed: true,
            });
        }
    }
    for version in &versions {
        resources.refresh_runtime(version.clone());
    }
    Ok(versions)
}

#[tauri::command]
pub(crate) fn list_installed_dsh_versions(
    resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<String>, String> {
    Ok(resources
        .snapshot()?
        .versions
        .into_iter()
        .filter(|version| version.installed)
        .map(|version| version.name)
        .collect())
}

pub(crate) fn install_dsh_version_with_cancel(
    version: String,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> Result<BoxConfig, String> {
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .clone()
        .ok_or("DSH Box storage is not configured")?;
    let destination = dsh_version_directory(&root, &version);
    if destination.exists() {
        return Err(format!(
            "DSH version already exists: {}",
            destination.display()
        ));
    }
    fs::create_dir_all(destination.parent().ok_or("invalid DSH destination")?)
        .map_err(|error| format!("cannot create DSH destination: {error}"))?;
    let commit = match shallow_clone_with_cancel(
        DSH_REPOSITORY,
        &destination,
        (version != "latest").then_some(version.as_str()),
        cancelled,
    ) {
        Ok(commit) => commit,
        Err(error) => {
            remove_checkout(&destination);
            return Err(error);
        }
    };
    fs::write(
        destination.join(".dshbox-runtime.json"),
        serde_json::json!({ "version": version, "commit": commit }).to_string(),
    )
    .map_err(|error| format!("cannot write runtime metadata: {error}"))?;
    let mut updated = config;
    updated.selected_dsh_version = Some(version);
    write_config(&updated)?;
    Ok(updated)
}

#[tauri::command]
pub(crate) fn uninstall_dsh_version(
    version: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    ensure_resource_idle(&manager, &format!("runtime:{version}"))?;
    let mut config = read_config()?;
    let root = config
        .runtime_directory
        .as_deref()
        .ok_or("DSH Box storage is not configured")?;
    let directory = dsh_version_directory(root, &version)
        .parent()
        .ok_or("invalid DSH destination")?
        .to_path_buf();
    if !directory.is_dir() {
        return Err(format!("DSH version is not installed: {version}"));
    }
    fs::remove_dir_all(&directory)
        .map_err(|error| format!("cannot remove {}: {error}", directory.display()))?;
    if config.selected_dsh_version.as_deref() == Some(&version) {
        config.selected_dsh_version = None;
    }
    write_config(&config)?;
    refresh_global_state(&app);
    Ok(config)
}
