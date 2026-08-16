use super::super::*;
use box_dsh_versions::{ensure_base_template, HarnessUpgradeReport};

#[tauri::command]
pub(crate) fn list_dsh_versions(
    resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<DshVersion>, String> {
    let client = connect()?;
    // The daemon catalog already merges catalog + installed versions;
    // `latest` is a virtual entry the UI always shows.
    let names: Vec<String> = serde_json::from_value(call(
        &client,
        "list_dsh_catalog",
        serde_json::json!({}),
    )?)
    .map_err(|error| format!("invalid version catalog: {error}"))?;
    let installed: Vec<String> = serde_json::from_value(call(
        &client,
        "list_installed_dsh_versions",
        serde_json::json!({}),
    )?)
    .map_err(|error| format!("invalid installed versions: {error}"))?;
    let mut versions = vec![DshVersion {
        name: "latest".to_owned(),
        installed: installed.contains(&"latest".to_owned()),
    }];
    versions.extend(names.into_iter().filter(|name| name != "latest").map(|name| {
        DshVersion {
            installed: installed.contains(&name),
            name,
        }
    }));
    for version in &versions {
        resources.refresh_runtime(version.clone());
    }
    Ok(versions)
}

/// Explicitly run the legacy-data migration and report what changed per
/// installed harness (metadata, `.dboxfile`, base template).
#[tauri::command]
pub(crate) fn upgrade_legacy_resources() -> Result<Vec<HarnessUpgradeReport>, String> {
    let client = connect()?;
    let value = call(&client, "upgrade_legacy_resources", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid upgrade report: {error}"))
}

#[tauri::command]
pub(crate) fn list_installed_dsh_versions(
    _resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<String>, String> {
    let client = connect()?;
    let value = call(&client, "list_installed_dsh_versions", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid installed versions: {error}"))
}

#[allow(dead_code)] // Phase 4 removes this after all enqueue commands go RPC
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
        &mirror_url(DSH_REPOSITORY, config.github_mirror.as_deref()),
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
    // Every installed harness gets a base template (plugin aggregator) so
    // containers can be created from it right away.
    if ensure_base_template(&root, &version) {
        eprintln!(
            "generated base template {}",
            box_dsh_versions::harness_template_path(&root, &version).display()
        );
    }
    let mut updated = config;
    updated.selected_dsh_version = Some(version);
    write_config(&updated)?;
    Ok(updated)
}

#[tauri::command]
pub(crate) fn uninstall_dsh_version(
    version: String,
    _manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    let client = connect()?;
    call(
        &client,
        "uninstall_dsh_version",
        serde_json::json!({ "version": version }),
    )?;
    let config = read_config()?;
    refresh_global_state(&app);
    Ok(config)
}
