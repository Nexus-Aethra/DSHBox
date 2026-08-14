use super::*;

#[allow(dead_code)]
pub(crate) fn managed_npm_script(root: &str) -> Option<PathBuf> {
    let candidate =
        PathBuf::from(root).join("tools/node/current/lib/node_modules/npm/bin/npm-cli.js");
    candidate.is_file().then_some(candidate)
}

pub(crate) fn detect_toolchain(id: &str, name: &str) -> ToolchainStatus {
    let version = bundled_runtime().ok().and_then(|runtime| match id {
        "node" => Some(runtime.node_version.clone()),
        "npm" => Some(runtime.npm_version.clone()),
        "pnpm" => Some(runtime.pnpm_version.clone()),
        _ => None,
    });
    ToolchainStatus {
        id: id.to_owned(),
        name: name.to_owned(),
        system_version: None,
        managed_version: version,
    }
}

pub(crate) fn scan_toolchains(_: &BoxConfig) -> Vec<ToolchainStatus> {
    [
        detect_toolchain("node", "Node.js"),
        detect_toolchain("npm", "npm"),
        detect_toolchain("pnpm", "pnpm"),
    ]
    .into()
}

pub(crate) fn resolve_toolchain(id: &str) -> Result<ResolvedToolchain, String> {
    if !is_known_toolchain(id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    let runtime = bundled_runtime()?;
    let (path, arguments) = match id {
        "node" => (runtime.node.clone(), Vec::new()),
        "npm" => (
            runtime.node.clone(),
            vec![runtime.npm.to_string_lossy().into_owned()],
        ),
        "pnpm" => (
            runtime.node.clone(),
            vec![runtime.pnpm.to_string_lossy().into_owned()],
        ),
        _ => return Err(format!("unsupported bundled toolchain: {id}")),
    };
    Ok(ResolvedToolchain {
        id: id.to_owned(),
        source: "bundled".to_owned(),
        path: path.to_string_lossy().into_owned(),
        arguments,
    })
}

pub(crate) fn command_for_toolchain(toolchain: &ResolvedToolchain) -> Command {
    let mut command = Command::new(&toolchain.path);
    suppress_console_window(&mut command);
    command.args(&toolchain.arguments);
    // Prepend the bundled runtime bin directories so child processes can
    // resolve bare `pnpm`/`npm` commands: pnpm's dependency-status check and
    // DSH build scripts spawn them without the bundled runtime on PATH.
    if let Ok(runtime) = bundled_runtime() {
        // node and pnpm live as siblings under runtime/<target>/, e.g.
        // node/node.exe and pnpm/pnpm.cmd. The recorded tool entries are
        // deep paths (node/node.exe, pnpm/node_modules/pnpm/bin/pnpm.mjs),
        // so derive the bin directories from the node executable's parent.
        if let Some(node_dir) = runtime.node.parent() {
            let pnpm_dir = node_dir.parent().map(|root| root.join("pnpm"));
            if let Some(existing) = env::var_os("PATH") {
                let mut parts: Vec<OsString> = vec![node_dir.as_os_str().to_owned()];
                if let Some(pnpm_dir) = pnpm_dir {
                    parts.push(pnpm_dir.as_os_str().to_owned());
                }
                parts.push(existing);
                if let Ok(joined) = env::join_paths(parts) {
                    command.env("PATH", joined);
                }
            }
        }
    }
    // Apply the user-configured npm registry to every npm/pnpm child so
    // installs and DSH build scripts resolve packages through the mirror.
    if let Ok(config) = read_config() {
        if let Some(registry) = config.npm_registry.as_deref() {
            command.env("npm_config_registry", registry);
        }
    }
    command
}

pub(crate) fn wait_for_process(
    child: &mut Child,
    task: Option<&TaskContext>,
    description: &str,
) -> Result<std::process::ExitStatus, String> {
    loop {
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            kill_process_tree(child.id());
            let _ = child.wait();
            return Err(format!("task cancelled while {description}"));
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Spawns a child with piped output and forwards every line to both the
/// given log file and the task's live log, so the UI shows progress instead
/// of appearing stuck during long installs/builds.
pub(crate) fn spawn_forwarding_log(
    command: &mut Command,
    log_file: &Path,
    task: Option<&TaskContext>,
) -> Result<Child, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("missing piped stdout")?;
    let stderr = child.stderr.take().ok_or("missing piped stderr")?;
    let log_file = log_file.to_path_buf();
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let task = task.cloned();
        let log_file = log_file.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let _ = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_file)
                    .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
                if let Some(task) = &task {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        task.log(trimmed);
                    }
                }
                line.clear();
            }
        });
    }
    Ok(child)
}

#[allow(dead_code)]
pub(crate) fn node_platform() -> Result<String, String> {
    let os = match env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => return Err(format!("managed Node is not yet supported on {other}")),
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => return Err(format!("managed Node is not yet supported on {other}")),
    };
    Ok(format!("{os}-{arch}"))
}

#[allow(dead_code)]
pub(crate) fn install_managed_node(root: &str) -> Result<ToolchainInstallStatus, String> {
    let platform = node_platform()?;
    let releases: Vec<NodeRelease> = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .build()
        .map_err(|error| error.to_string())?
        .get("https://nodejs.org/dist/index.json")
        .send()
        .map_err(|error| format!("cannot download Node index: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .map_err(|error| format!("cannot parse Node index: {error}"))?;
    let release = releases
        .into_iter()
        .find(|release| {
            release.version == DEFAULT_NODE_VERSION && release.files.contains(&platform)
        })
        .ok_or_else(|| {
            format!("Box-compatible Node {DEFAULT_NODE_VERSION} is unavailable for {platform}")
        })?;
    let archive_url = format!(
        "https://nodejs.org/dist/{0}/node-{0}-{1}.tar.xz",
        release.version, platform
    );
    let logs = PathBuf::from(root).join("logs").join("toolchains");
    fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
    let log_path = logs.join(format!("node-{}.log", release.version));
    let mut lines = vec![
        format!("downloading Node {}", release.version),
        archive_url.clone(),
    ];
    let archive = reqwest::blocking::get(&archive_url)
        .map_err(|error| format!("cannot download Node: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .bytes()
        .map_err(|error| error.to_string())?;
    lines.push("extracting archive".to_owned());
    let tools = PathBuf::from(root).join("tools").join("node");
    fs::create_dir_all(&tools).map_err(|error| error.to_string())?;
    let temporary = tools.join(format!(".{}.tmp", release.version));
    if temporary.exists() {
        fs::remove_dir_all(&temporary).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&temporary).map_err(|error| error.to_string())?;
    tar::Archive::new(XzDecoder::new(archive.as_ref()))
        .unpack(&temporary)
        .map_err(|error| format!("cannot unpack Node: {error}"))?;
    let extracted = temporary.join(format!("node-{}-{}", release.version, platform));
    let current = tools.join("current");
    if current.exists() {
        fs::remove_dir_all(&current).map_err(|error| error.to_string())?;
    }
    fs::rename(&extracted, &current).map_err(|error| format!("cannot install Node: {error}"))?;
    let _ = fs::remove_dir_all(&temporary);
    lines.push(format!("installed Node {}", release.version));
    fs::write(&log_path, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())?;
    Ok(ToolchainInstallStatus {
        id: "node".to_owned(),
        stage: "ready".to_owned(),
        log_path: log_path.to_string_lossy().into_owned(),
        lines,
    })
}

#[allow(dead_code)]
pub(crate) fn install_managed_pnpm(root: &str) -> Result<ToolchainInstallStatus, String> {
    let config = read_config()?;
    if config.toolchain_sources.get("npm").map(String::as_str) == Some("managed")
        && managed_npm_script(root).is_none()
    {
        install_managed_node(root)?;
    }
    let npm = resolve_toolchain("npm")?;
    let logs = PathBuf::from(root).join("logs").join("toolchains");
    fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
    let log_path = logs.join(format!("pnpm-{DEFAULT_PNPM_VERSION}.log"));
    let prefix = PathBuf::from(root)
        .join("tools/pnpm")
        .join(DEFAULT_PNPM_VERSION);
    fs::create_dir_all(&prefix).map_err(|error| error.to_string())?;
    let mut lines = vec![format!(
        "installing pnpm@{DEFAULT_PNPM_VERSION} with {}",
        npm.path
    )];
    let arguments = vec![
        "install".to_owned(),
        "--prefix".to_owned(),
        prefix.to_string_lossy().into_owned(),
        "--cache".to_owned(),
        PathBuf::from(root)
            .join("store/npm-cache")
            .to_string_lossy()
            .into_owned(),
        format!("pnpm@{DEFAULT_PNPM_VERSION}"),
    ];
    let output = command_for_toolchain(&npm)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run npm: {error}"))?;
    lines.push(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    lines.push(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    if !output.status.success() {
        return Err(format!(
            "npm failed while installing pnpm; inspect {}",
            log_path.display()
        ));
    }
    lines.push(format!("installed pnpm@{DEFAULT_PNPM_VERSION}"));
    fs::write(&log_path, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())?;
    Ok(ToolchainInstallStatus {
        id: "pnpm".to_owned(),
        stage: "ready".to_owned(),
        log_path: log_path.to_string_lossy().into_owned(),
        lines,
    })
}

#[tauri::command]
pub(crate) fn start_toolchain_install(id: String) -> Result<ToolchainInstallStatus, String> {
    if !is_known_toolchain(&id) {
        return Err(format!("unsupported toolchain: {id}"));
    }
    Err("Node, npm, and pnpm are bundled with DSH Box; reinstall the application to repair the runtime".to_owned())
}

#[tauri::command]
pub(crate) fn enqueue_toolchain_install(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "toolchain-install",
        vec![format!("toolchain:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Preparing toolchain installation", 5);
        task.check_cancelled()?;
        task.log("starting toolchain installer");
        let result = start_toolchain_install(id).map(|_| ());
        task.update("Finalizing toolchain installation", 95);
        result
    });
    Ok(task)
}
