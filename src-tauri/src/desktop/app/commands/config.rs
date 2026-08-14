use super::super::*;

#[tauri::command]
pub(crate) fn load_config() -> Result<BoxConfig, String> {
    read_config()
}

#[tauri::command]
pub(crate) fn save_runtime_directory(
    directory: String,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    let selected = PathBuf::from(&directory);
    if !selected.is_dir() {
        return Err(format!(
            "runtime directory does not exist: {}",
            selected.display()
        ));
    }
    // Persist the canonical absolute path: a drive-relative value like `D:`
    // would turn every later join into `D:containers`, which resolves against
    // each child process's current drive and crashes bundled Node/pnpm.
    let normalized = normalize_runtime_directory(&directory)?;
    let mut config = read_config()?;
    config.runtime_directory = Some(normalized);
    write_config(&config)?;
    refresh_global_state(&app);
    Ok(config)
}

#[tauri::command]
pub(crate) fn save_language(language: String, app: tauri::AppHandle) -> Result<BoxConfig, String> {
    if language != "en" && language != "zh-CN" {
        return Err("unsupported language".to_owned());
    }
    let mut config = read_config()?;
    config.language = language;
    write_config(&config)?;
    refresh_global_state(&app);
    Ok(config)
}

#[tauri::command]
pub(crate) fn save_mirror_settings(
    github_mirror: Option<String>,
    npm_registry: Option<String>,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    let mut config = read_config()?;
    config.github_mirror = normalize_optional_url(github_mirror);
    config.npm_registry = normalize_optional_url(npm_registry);
    write_config(&config)?;
    refresh_global_state(&app);
    Ok(config)
}
