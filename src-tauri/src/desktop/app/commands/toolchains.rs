use super::super::*;

#[tauri::command(async)]
pub(crate) fn detect_toolchains(
    _resources: tauri::State<'_, ResourceStateManager>,
) -> Result<Vec<ToolchainStatus>, String> {
    let client = connect()?;
    let value = call(&client, "detect_toolchains", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid toolchain list: {error}"))
}

#[tauri::command(async)]
pub(crate) fn save_toolchain_source(
    id: String,
    source: String,
    app: tauri::AppHandle,
) -> Result<BoxConfig, String> {
    if !is_known_toolchain(&id) || !["system", "managed"].contains(&source.as_str()) {
        return Err("unsupported toolchain source".to_owned());
    }
    let mut config = read_config()?;
    config.toolchain_sources.insert(id, source);
    write_config(&config)?;
    refresh_global_state(&app);
    Ok(config)
}
