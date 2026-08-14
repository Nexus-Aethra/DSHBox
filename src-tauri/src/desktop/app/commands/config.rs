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
    let mut config = read_config()?;
    config.runtime_directory = Some(directory);
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
