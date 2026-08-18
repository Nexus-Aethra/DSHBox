use super::super::*;

#[tauri::command]
pub(crate) fn detect_toolchains(
    _resources: tauri::State<ResourceStateManager>,
) -> Result<Vec<ToolchainStatus>, String> {
    let client = connect()?;
    let value = call(&client, "detect_toolchains", serde_json::json!({}))?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid toolchain list: {error}"))
}

#[tauri::command]
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

#[tauri::command]
pub(crate) fn resolve_toolchain_command(id: String) -> Result<ResolvedToolchain, String> {
    resolve_toolchain(&id)
}

#[tauri::command]
pub(crate) fn run_toolchain_command(
    request: ToolchainCommandRequest,
) -> Result<ToolchainCommandResult, String> {
    let toolchain = resolve_toolchain(&request.id)?;
    let mut command = command_for_toolchain(&toolchain);
    command.args(&request.args);
    if let Some(cwd) = request.cwd {
        let directory = Path::new(&cwd);
        if !directory.is_dir() {
            return Err(format!(
                "working directory does not exist: {}",
                directory.display()
            ));
        }
        command.current_dir(directory);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run {}: {error}", toolchain.path))?;
    Ok(ToolchainCommandResult {
        path: toolchain.path,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}
