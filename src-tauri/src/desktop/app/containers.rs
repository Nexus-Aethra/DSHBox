use super::*;

#[tauri::command]
pub(crate) fn create_dsh_container(
    request: CreateDshContainerRequest,
    app: tauri::AppHandle,
) -> Result<DshContainer, String> {
    let name = request.name.trim().to_owned();
    let version = request.version;
    let profile = request.profile.trim().to_owned();
    if !is_safe_version_name(&version) {
        return Err("invalid DSH version".to_owned());
    }
    if name.is_empty() || name.len() > 80 {
        return Err("container name must contain 1 to 80 characters".to_owned());
    }
    if !is_safe_identifier(&profile) {
        return Err("profile must use letters, numbers, dots, dashes, or underscores".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    if !dsh_version_directory(&root, &version).join(".git").is_dir() {
        return Err(format!("DSH version is not installed: {version}"));
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let id = format!("container-{timestamp}");
    let directory = PathBuf::from(&root).join("instances").join(&id);
    for name in ["profile", "workspace", "logs", "state"] {
        fs::create_dir_all(directory.join(name))
            .map_err(|error| format!("cannot create container: {error}"))?;
    }
    create_profile_manifest(&directory, &profile)?;
    let metadata = serde_json::json!({ "id": id, "name": name, "version": version, "profile": profile, "source": dsh_version_directory(&root, &version) });
    fs::write(
        directory.join("container.json"),
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write container metadata: {error}"))?;
    let container = DshContainer {
        id,
        name,
        version,
        profile,
        directory: directory.to_string_lossy().into_owned(),
        status: "stopped".to_owned(),
    };
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

pub(crate) fn ensure_container_workspace(directory: &Path) -> Result<PathBuf, String> {
    let workspace = directory.join("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("cannot create container workspace: {error}"))?;
    Ok(workspace)
}

pub(crate) fn write_dshbox_context_patch(
    directory: &Path,
    container: &serde_json::Value,
    profile: &str,
) -> Result<PathBuf, String> {
    let workspace = ensure_container_workspace(directory)?;
    let container_name = container["name"].as_str().unwrap_or("DSH Container");
    let container_id = container["id"].as_str().unwrap_or("unknown");
    let version = container["version"].as_str().unwrap_or("unknown");
    let clean = |value: &str| value.replace(['\r', '\n'], " ");
    let context = format!(
        "You are a coding agent powered by the {{{{model}}}} model. Your working directory is {{{{cwd}}}}.\n\nDSH Box context:\n- You are working inside Container: {} (ID: {}, DSH: {}, Profile: {}).\n- Workspace: {}\n- DSH profile home: {}\n- Container plugins: {}\n- Container skills: {}\n- Container logs: {}\n\nKeep project and creation-mode changes in the current workspace. Keep profile, plugin, and Skill changes within this Container. Import external plugins only through DSH Box Plugin Repo. Do not modify another Container or system paths unless the user explicitly asks you to do so.",
        clean(container_name), clean(container_id), clean(version), clean(profile),
        workspace.display(), directory.join("profile").display(), directory.join("extensions/plugins").display(), directory.join("profile/skills").display(), directory.join("logs").display(),
    );
    let yaml_persona = context
        .lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let patch = directory.join("state/dshbox-context.patch.yml");
    fs::write(
        &patch,
        format!("# Generated by dshbox. This file is replaced on every Container start.\n- id: system-prompt\n  config:\n    persona: |-\n{yaml_persona}\n"),
    )
    .map_err(|error| format!("cannot write DSH Box context patch: {error}"))?;
    Ok(patch)
}

/// Repairs Box-created, empty named profiles from builds before profile templates were persisted.
pub(crate) fn repair_known_profile_template(container_directory: &Path, profile: &str) -> Result<(), String> {
    if !matches!(profile, "web" | "headless") {
        return Ok(());
    }
    let directory = container_directory.join("profile/profiles").join(profile);
    let manifest_path = directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("cannot read profile: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile: {error}"))?;
    let empty = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        manifest["dsh"]["profile"]["bundles"] =
            serde_json::json!(profile_template_bundles(profile));
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("cannot repair profile: {error}"))?;
    }
    write_profile_support_files(&directory)
}

/// Ensures every non-bundled DSH plugin selected by a profile has its declared runtime entry.
/// GitHub and tarball imports may contain TypeScript sources, so this prepares those sources
/// before the DSH loader attempts to import them.
pub(crate) fn preflight_profile_plugins(
    container_directory: &Path,
    profile: &str,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    let profile_directory = container_directory.join("profile/profiles").join(profile);
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(profile_directory.join("package.json"))
            .map_err(|error| format!("cannot read profile manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile manifest: {error}"))?;
    let bundles = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or("profile manifest has no dsh.profile.bundles")?;
    for bundle in bundles.iter().filter_map(serde_json::Value::as_str) {
        if bundle.starts_with("@deepseek-ai/") {
            continue;
        }
        let plugin_directory = profile_directory.join("node_modules").join(bundle);
        let plugin_manifest_path = plugin_directory.join("package.json");
        if !plugin_manifest_path.is_file() {
            return Err(format!(
                "profile plugin {bundle} is not installed; re-add it from Container details"
            ));
        }
        let plugin_manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&plugin_manifest_path)
                .map_err(|error| format!("cannot read plugin {bundle} manifest: {error}"))?,
        )
        .map_err(|error| format!("cannot parse plugin {bundle} manifest: {error}"))?;
        let Some(entry) = plugin_runtime_entry(&plugin_manifest) else {
            continue;
        };
        if plugin_directory.join(&entry).is_file() {
            continue;
        }
        if let Some(task) = task {
            task.update(format!("Preparing plugin {bundle}"), 32);
            task.log(&format!(
                "plugin {bundle} entry {entry} is missing; installing dependencies and building its source"
            ));
            prepare_plugin_source(&plugin_directory, bundle, &entry, task)?;
        } else {
            return Err(format!(
                "plugin {bundle} has no built entry {entry}; start it from DSH Box so it can be prepared"
            ));
        }
    }
    Ok(())
}

pub(crate) fn plugin_runtime_entry(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("main")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .pointer("/exports/./default")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

pub(crate) fn prepare_plugin_source(
    directory: &Path,
    name: &str,
    entry: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.task_id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let frozen = if directory.join("pnpm-lock.yaml").is_file() {
        "--frozen-lockfile"
    } else {
        "--no-frozen-lockfile"
    };
    let mut install = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            directory.to_string_lossy().as_ref(),
            "install",
            frozen,
        ])
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .spawn()
        .map_err(|error| format!("cannot install dependencies for plugin {name}: {error}"))?;
    let status = wait_for_process(&mut install, Some(task), "installing plugin dependencies")?;
    if !status.success() {
        return Err(format!(
            "plugin {name} dependency installation exited with {status}"
        ));
    }
    if directory.join(entry).is_file() {
        return Ok(());
    }
    if plugin_has_script(directory, "build")? {
        task.update(format!("Building plugin {name}"), 38);
        let mut build = command_for_toolchain(&pnpm)
            .args([
                "--dir",
                directory.to_string_lossy().as_ref(),
                "run",
                "build",
            ])
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|error| format!("cannot build plugin {name}: {error}"))?;
        let status = wait_for_process(&mut build, Some(task), "building plugin")?;
        if !status.success() {
            return Err(format!("plugin {name} build exited with {status}"));
        }
    }
    if directory.join(entry).is_file() {
        Ok(())
    } else {
        Err(format!(
            "plugin {name} build completed but did not create its declared entry {entry}"
        ))
    }
}

pub(crate) fn plugin_has_script(directory: &Path, script: &str) -> Result<bool, String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(directory.join("package.json"))
            .map_err(|error| format!("cannot read plugin manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin manifest: {error}"))?;
    Ok(manifest
        .pointer(&format!("/scripts/{script}"))
        .and_then(serde_json::Value::as_str)
        .is_some())
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
    manager: tauri::State<ContainerManager>,
    tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !is_safe_version_name(&id) {
        return Err("invalid container id".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(&id);
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = PathBuf::from(root).join("instances").join(&id);
    if !directory.is_dir() {
        return Err(format!("container not found: {id}"));
    }
    fs::remove_dir_all(directory).map_err(|error| format!("cannot remove container: {error}"))?;
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
mod context_patch_tests {
    use super::*;

    #[test]
    fn generated_context_patch_names_the_container_workspace_and_profile() {
        let root = std::env::temp_dir().join(format!("dshbox-context-{}", now_seconds()));
        fs::create_dir_all(root.join("state")).unwrap();
        let metadata = serde_json::json!({
            "id": "container-1",
            "name": "Example",
            "version": "latest",
        });
        let patch = write_dshbox_context_patch(&root, &metadata, "web").unwrap();
        let content = fs::read_to_string(patch).unwrap();
        assert!(root.join("workspace").is_dir());
        assert!(content.contains("Container: Example (ID: container-1, DSH: latest, Profile: web)"));
        assert!(content.contains("- id: system-prompt"));
        fs::remove_dir_all(root).unwrap();
    }
}
