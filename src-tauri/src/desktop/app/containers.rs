use super::*;

#[tauri::command(async)]
pub(crate) fn create_dsh_container(
    request: CreateDshContainerRequest,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    let client = connect()?;
    let value = call(
        &client,
        "create_container",
        serde_json::json!({ "name": request.name, "version": request.version, "profile": request.profile }),
    )?;
    let container: DshContainer = serde_json::from_value(value)
        .map_err(|error| format!("invalid container record: {error}"))?;
    refresh_global_state(&app);
    Ok(container)
}

pub(crate) fn create_profile_manifest(container_directory: &Path, profile: &str) -> Result<(), String> {
    let directory = container_directory.join("profile/profiles").join(profile);
    if directory.exists() {
        return Err(format!("profile already exists: {profile}"));
    }
    fs::create_dir_all(&directory).map_err(|error| format!("cannot create profile: {error}"))?;
    let manifest = serde_json::json!({
        "name": format!("dsh-profile-{profile}"),
        "private": true,
        "dependencies": {},
        "dsh": { "profile": { "bundles": profile_template_bundles(profile) } }
    });
    fs::write(
        directory.join("package.json"),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write profile manifest: {error}"))?;
    write_profile_support_files(&directory)
}

pub(crate) fn profile_template_bundles(profile: &str) -> Vec<&'static str> {
    match profile {
        "web" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        "headless" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        _ => vec!["@deepseek-ai/dsh-base"],
    }
}

pub(crate) fn write_profile_support_files(directory: &Path) -> Result<(), String> {
    let patch = directory.join("cordis.patch.yml");
    if !patch.exists() {
        fs::write(&patch, "# User overrides for this DSH profile.\n[]\n")
            .map_err(|error| format!("cannot write profile patch: {error}"))?;
    }
    let workspace = directory.join("pnpm-workspace.yaml");
    if !workspace.exists() {
        fs::write(
            &workspace,
            "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n",
        )
        .map_err(|error| format!("cannot write profile workspace: {error}"))?;
    }
    Ok(())
}



#[tauri::command(async)]
pub(crate) fn add_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<'_, TaskManager>,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    if !directory.join("container.json").is_file() {
        return Err(format!("container not found: {id}"));
    }
    create_profile_manifest(&directory, &profile)?;
    refresh_global_state(&app);
    app.state::<ResourceStateManager>()
        .snapshot()?
        .containers
        .into_iter()
        .find(|container| container.id == id)
        .ok_or("container disappeared after profile creation".to_owned())
}

#[tauri::command(async)]
pub(crate) fn set_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<'_, TaskManager>,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata_path = directory.join("container.json");
    let mut metadata: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|error| format!("cannot read container: {error}"))?,
    )
    .map_err(|error| format!("cannot parse container: {error}"))?;
    if !directory
        .join("profile/profiles")
        .join(&profile)
        .join("package.json")
        .is_file()
    {
        return Err(format!("profile not found: {profile}"));
    }
    metadata["profile"] = serde_json::Value::String(profile);
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot save container: {error}"))?;
    refresh_global_state(&app);
    app.state::<ResourceStateManager>()
        .snapshot()?
        .containers
        .into_iter()
        .find(|container| container.id == id)
        .ok_or("container disappeared after profile update".to_owned())
}

#[tauri::command(async)]
pub(crate) fn delete_dsh_container(
    id: String,
    _manager: tauri::State<'_, ContainerManager>,
    _tasks: tauri::State<'_, TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !is_safe_version_name(&id) {
        return Err("invalid container id".to_owned());
    }
    // The daemon stops the container and removes its directory; the desktop
    // only needs to close the container's front window locally.
    if let Some(window) = app.get_webview_window(&format!("dsh-front-{id}")) {
        let _ = window.close();
    }
    let client = connect()?;
    call(&client, "delete_container", serde_json::json!({ "id": id }))?;
    refresh_global_state(&app);
    Ok(())
}

#[tauri::command(async)]
pub(crate) fn read_container_log(id: String, log: String) -> Result<String, String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    let filename = match log.as_str() {
        "host" => "host.log",
        "rebuild" => "rebuild.log",
        "webview" => "webview.log",
        _ => return Err("unsupported container log".to_owned()),
    };
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = container_directory(&root, &id).join("logs").join(filename);
    if !path.is_file() {
        return Ok(format!(
            "No {log} log has been created for this container yet."
        ));
    }
    fs::read_to_string(path).map_err(|error| format!("cannot read container log: {error}"))
}

#[tauri::command(async)]
pub(crate) fn append_container_webview_log(
    window: tauri::WebviewWindow,
    id: String,
    line: String,
) -> Result<(), String> {
    if window.label() != format!("dsh-front-{id}") || !is_safe_identifier(&id) {
        return Err("webview is not authorized to write this container log".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = container_directory(&root, &id).join("logs/webview.log");
    let message = line.replace(['\n', '\r', '\0'], " ");
    let message = message.chars().take(8_000).collect::<String>();
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            std::io::Write::write_all(
                &mut file,
                format!("[{}] {message}\n", now_seconds()).as_bytes(),
            )
        })
        .map_err(|error| format!("cannot write webview log: {error}"))
}

#[allow(dead_code)]
pub(crate) fn webview_log_script(id: &str) -> String {
    let id = serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        r#"(() => {{
  const report = (kind, value) => {{
    try {{ window.__TAURI_INTERNALS__.invoke('append_container_webview_log', {{ id: {id}, line: `${{kind}}: ${{String(value)}}` }}).catch(() => undefined) }} catch (_) {{}}
  }};
  window.addEventListener('error', (event) => report('error', `${{event.message}} at ${{event.filename}}:${{event.lineno}}:${{event.colno}}`));
  window.addEventListener('unhandledrejection', (event) => report('unhandledrejection', event.reason?.stack || event.reason));
  const originalError = console.error.bind(console);
  console.error = (...args) => {{ report('console.error', args.map(String).join(' ')); originalError(...args); }};
}})();"#
    )
}
