use super::*;

#[tauri::command]
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

/// Ensure the container's workspace directory exists.
///
/// Only used by the context-snapshot tests below; the daemon owns container
/// lifecycle now.
#[allow(dead_code)] // kept for the context_snapshot_tests below
pub(crate) fn ensure_container_workspace(directory: &Path) -> Result<PathBuf, String> {
    let workspace = directory.join("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("cannot create container workspace: {error}"))?;
    Ok(workspace)
}

/// Render the per-container JSON snapshot Box writes on every container start.
///
/// The snapshot becomes a `dsh-box:container` PromptContext section (order 130)
/// that the agent receives as a user-role history snapshot. Returning the
/// absolute paths lets the lifecycle wiring mount both the snapshot and the
/// Cordis patch overlay that points at it.
///
/// Only used by the context-snapshot tests below; the daemon renders the
/// snapshots for live containers now.
#[allow(dead_code)] // kept for the context_snapshot_tests below
pub(crate) fn write_dshbox_context_snapshot(
    directory: &Path,
    container: &serde_json::Value,
    profile: &str,
    _dshbox_home: &Path,
) -> Result<DshContextFiles, String> {
    let workspace = ensure_container_workspace(directory)?;
    let container_name = container["name"].as_str().unwrap_or("DSH Container");
    let container_id = container["id"].as_str().unwrap_or("unknown");
    let version = container["version"].as_str().unwrap_or("unknown");
    let profile_home = directory.join("profile");
    let plugins_root = directory.join("extensions/plugins");
    let skills_root = directory.join("profile/skills");
    let logs_root = directory.join("logs");

    // Read the env-var names Box already wrote into the container's
    // .credentials.yaml via the DSH settings UI. We only ship the names
    // into the snapshot; the actual values stay where the user put them.
    let api_key_envs = read_credentials_env_names(&profile_home);

    let state_dir = directory.join("state");
    fs::create_dir_all(&state_dir)
        .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;
    let snapshot_path = state_dir.join(SNAPSHOT_FILENAME);
    let patch_path = state_dir.join(PATCH_FILENAME);

    let snapshot_body = render_snapshot(
        container_id,
        container_name,
        version,
        profile,
        &workspace,
        &profile_home,
        &plugins_root,
        &skills_root,
        &logs_root,
        _dshbox_home,
        &api_key_envs,
    );
    // Atomic write: stage to .tmp then rename so a racing read never sees a
    // half-written snapshot.
    let snapshot_tmp = snapshot_path.with_extension("json.tmp");
    fs::write(&snapshot_tmp, snapshot_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", snapshot_tmp.display()))?;
    fs::rename(&snapshot_tmp, &snapshot_path)
        .map_err(|error| format!("cannot rename {}: {error}", snapshot_tmp.display()))?;

    let patch_body = render_patch_yml(&snapshot_path, DEFAULT_ORDER);
    let patch_tmp = patch_path.with_extension("yml.tmp");
    fs::write(&patch_tmp, patch_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", patch_tmp.display()))?;
    fs::rename(&patch_tmp, &patch_path)
        .map_err(|error| format!("cannot rename {}: {error}", patch_tmp.display()))?;

    Ok(DshContextFiles { snapshot_path, patch_path })
}

/// Extract the `apiKeyEnv` names that the DSH settings UI wrote into
/// `<DSH_HOME>/.credentials.yaml`. Tolerant of missing or malformed
/// files: a container that has not configured providers yet still gets a
/// valid snapshot with an empty providers list.
#[allow(dead_code)] // kept for the context_snapshot_tests below
fn read_credentials_env_names(profile_home: &Path) -> Vec<String> {
    let path = profile_home.join(".credentials.yaml");
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return Vec::new(),
    };
    let value: serde_yaml::Value = match serde_yaml::from_str(&body) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    if let Some(map) = value.as_mapping() {
        for (key, _) in map {
            if let Some(key) = key.as_str() {
                names.push(key.to_owned());
            }
        }
    }
    names.sort();
    names
}


#[tauri::command]
pub(crate) fn add_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<TaskManager>,
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

#[tauri::command]
pub(crate) fn set_dsh_container_profile(
    id: String,
    profile: String,
    tasks: tauri::State<TaskManager>,
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

#[tauri::command]
pub(crate) fn delete_dsh_container(
    id: String,
    _manager: tauri::State<ContainerManager>,
    _tasks: tauri::State<TaskManager>,
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

#[tauri::command]
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

#[tauri::command]
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

#[cfg(test)]
mod context_snapshot_tests {
    use super::*;
    use box_dsh_context::{DEFAULT_ORDER, PATCH_FILENAME, SNAPSHOT_FILENAME};

    #[test]
    fn snapshot_writes_structured_json_and_cordis_patch() {
        let root = std::env::temp_dir().join(format!("dshbox-context-{}", now_seconds()));
        fs::create_dir_all(&root).unwrap();
        let metadata = serde_json::json!({
            "id": "container-1",
            "name": "Example",
            "version": "latest",
        });
        let files = write_dshbox_context_snapshot(&root, &metadata, "web", &root).unwrap();
        assert_eq!(files.snapshot_path, root.join("state").join(SNAPSHOT_FILENAME));
        assert_eq!(files.patch_path, root.join("state").join(PATCH_FILENAME));
        assert!(root.join("workspace").is_dir());

        let snapshot_body = fs::read_to_string(&files.snapshot_path).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot_body).unwrap();
        assert_eq!(snapshot["container"]["id"], "container-1");
        assert_eq!(snapshot["container"]["name"], "Example");
        assert_eq!(snapshot["container"]["version"], "latest");
        assert_eq!(snapshot["container"]["profile"], "web");
        assert!(snapshot["paths"]["workspace"].as_str().unwrap().ends_with("workspace"));
        assert_eq!(snapshot["paths"]["dshboxHome"], root.to_string_lossy().as_ref());
        assert_eq!(snapshot["credentials"]["providers"].as_array().unwrap().len(), 0);

        let patch_body = fs::read_to_string(&files.patch_path).unwrap();
        assert!(patch_body.contains("- insert:"));
        assert!(patch_body.contains("id: dsh-box-context"));
        assert!(patch_body.contains(format!("order: {DEFAULT_ORDER}").as_str()));
        assert!(patch_body.contains(&format!("contextFile: {}", files.snapshot_path.display())));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn snapshot_reads_credential_env_names_from_credentials_yaml() {
        let root = std::env::temp_dir().join(format!("dshbox-context-creds-{}", now_seconds()));
        let profile_home = root.join("profile");
        fs::create_dir_all(&profile_home).unwrap();
        fs::write(
            profile_home.join(".credentials.yaml"),
            "DEEPSEEK_API_KEY: sk-test\nMINIMAX_CN_API_KEY: sk-test2\n",
        )
        .unwrap();
        let metadata = serde_json::json!({
            "id": "container-creds",
            "name": "WithCreds",
            "version": "latest",
        });
        let files = write_dshbox_context_snapshot(&root, &metadata, "web", &root).unwrap();
        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&files.snapshot_path).unwrap()).unwrap();
        let providers = snapshot["credentials"]["providers"].as_array().unwrap();
        let envs: Vec<&str> = providers
            .iter()
            .map(|p| p["apiKeyEnv"].as_str().unwrap())
            .collect();
        // Sorted alphabetically by read_credentials_env_names.
        assert_eq!(envs, vec!["DEEPSEEK_API_KEY", "MINIMAX_CN_API_KEY"]);
        fs::remove_dir_all(&root).unwrap();
    }
}