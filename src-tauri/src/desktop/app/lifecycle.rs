use super::*;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

#[tauri::command]
pub(crate) fn enqueue_container_start(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "container-start",
        vec![format!("container:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Starting DSH host", 10);
        task.check_cancelled()?;
        start_dsh_container_with_task(id, work_app.state::<ContainerManager>(), Some(&task))
            .map(|_| ())
    });
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_container_stop(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let task = queue_task(
        &manager,
        &app,
        "container-stop",
        vec![format!("container:{id}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Stopping DSH host", 30);
        task.check_cancelled()?;
        stop_dsh_container(id, Some(&work_app), work_app.state::<ContainerManager>())
    });
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_container_rebuild(
    id: String,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let metadata = fs::read_to_string(container_directory(&root, &id).join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let version = serde_json::from_str::<serde_json::Value>(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?["version"]
        .as_str()
        .ok_or("container has no version")?
        .to_owned();
    let task = queue_task(
        &manager,
        &app,
        "container-rebuild",
        vec![format!("container:{id}"), format!("runtime:{version}")],
        serde_json::json!({ "id": id }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    let work_app = app.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        task.update("Rebuilding DSH runtime", 10);
        task.check_cancelled()?;
        rebuild_dsh_container_with_task(
            id,
            Some(&work_app),
            work_app.state::<ContainerManager>(),
            Some(&task),
        )
    });
    Ok(task)
}

pub(crate) fn start_dsh_container_with_task(
    id: String,
    manager: tauri::State<ContainerManager>,
    task: Option<&TaskContext>,
) -> Result<String, String> {
    if !is_safe_version_name(&id) {
        return Err("invalid container id".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    let version = value["version"]
        .as_str()
        .ok_or("container has no version")?;
    let profile = value["profile"].as_str().unwrap_or("web");
    repair_known_profile_template(&directory, profile)?;
    let workspace = ensure_container_workspace(&directory)?;
    let context_files = write_dshbox_context_snapshot(&directory, &value, profile)?;
    let source = dsh_version_directory(&root, version);
    if !source.join("package.json").is_file() {
        return Err("DSH source is incomplete".to_owned());
    }
    {
        let mut running = manager
            .running
            .lock()
            .map_err(|_| "container manager lock failed")?;
        if let Some(host) = running.get_mut(&id) {
            if host
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(host.url.clone());
            }
        }
    }
    if let Some(task) = task {
        task.update("Preparing DSH host", 25);
        task.check_cancelled()?;
    }
    preflight_profile_plugins(&directory, profile, task)?;
    if let Some(task) = task {
        task.check_cancelled()?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot allocate port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);
    let patch = directory.join("box-web.patch.yml");
    fs::write(
        &patch,
        format!("- id: webserver\n  config:\n    host: 127.0.0.1\n    port: {port}\n"),
    )
    .map_err(|error| format!("cannot write web patch: {error}"))?;
    let pnpm = resolve_toolchain("pnpm")?;
    let source_arg = source.to_string_lossy().into_owned();
    let url = format!("http://127.0.0.1:{port}");
    let log_path = directory.join("logs").join("host.log");
    // Smart scheduling: launch the existing frontend build directly when it
    // is present, and only compile when the artifact is missing or a launch
    // with a stale artifact fails (the explicit Rebuild button forces a
    // fresh build in every other case). Truncate the host log once per Start.
    let _ = fs::File::create(&log_path);
    let mut built = source
        .join("apps")
        .join("web")
        .join("dist")
        .join("index.html")
        .is_file();
    let mut attempt = 0;
    loop {
        attempt += 1;
        if !built {
            // A fresh DSH checkout has no dependencies; install them once
            // before the first build so `pnpm run build` can resolve tooling.
            if !source.join("node_modules").is_dir() {
                if let Some(task) = task {
                    task.update("Installing DSH dependencies", 40);
                    task.log("installing DSH dependencies");
                }
                let mut install = command_for_toolchain(&pnpm);
                install.args(["--dir", source_arg.as_ref(), "install"]);
                let mut install = spawn_forwarding_log(&mut install, &log_path, task)
                    .map_err(|error| format!("cannot install DSH dependencies: {error}"))?;
                let status = wait_for_process(&mut install, task, "installing DSH dependencies")?;
                if !status.success() {
                    return Err(format!(
                        "DSH dependency install failed; inspect {}",
                        log_path.display()
                    ));
                }
                if let Some(task) = task {
                    task.check_cancelled()?;
                }
            }
            if let Some(task) = task {
                task.update("Building DSH frontend", 55);
                task.log("building DSH frontend");
            }
            let mut build = command_for_toolchain(&pnpm);
            build.args(["--dir", source_arg.as_ref(), "run", "build"]);
            let mut build = spawn_forwarding_log(&mut build, &log_path, task)
                .map_err(|error| format!("cannot build DSH before launch: {error}"))?;
            let status = wait_for_process(&mut build, task, "building DSH frontend")?;
            if !status.success() {
                return Err(format!("DSH build failed; inspect {}", log_path.display()));
            }
            if let Some(task) = task {
                task.check_cancelled()?;
            }
            built = true;
        }
        if let Some(task) = task {
            task.update("Launching DSH host", 75);
            task.log("launching DSH host");
        }
        // Forward the host's live output to both host.log and the task log so
        // the panel shows startup details instead of looking stuck.
        // Surface the Cordis plugin tree vendored by commit 4 to DSH's
        // bundle resolver via NODE_PATH. DSH's app-boot/src/profile.ts
        // resolves bundles through createRequire().resolve.paths(), which
        // consults NODE_PATH as the final fallback.
        let plugins_node_modules = std::path::PathBuf::from(&root).join("plugins").join("node_modules");
        let mut command = command_for_toolchain(&pnpm);
        command
            .args([
                "--dir",
                source.to_string_lossy().as_ref(),
                "dsh",
                "--profile",
                profile,
                "--patch",
                context_files.patch_path.to_string_lossy().as_ref(),
                "--patch",
                patch.to_string_lossy().as_ref(),
            ])
            .current_dir(&workspace)
            .env("DSH_HOME", directory.join("profile"))
            .env("NODE_PATH", plugins_node_modules.as_os_str());
        let mut child = spawn_forwarding_log(&mut command, &log_path, task)
            .map_err(|error| format!("cannot start DSH host: {error}"))?;
        let ready = (0..80).any(|attempt| {
            if task.map(TaskContext::cancelled).unwrap_or(false) {
                kill_process_tree(child.id());
                let _ = child.wait();
                return false;
            }
            if child.try_wait().ok().flatten().is_some() {
                return false;
            }
            if let Some(task) = task {
                if attempt % 10 == 0 {
                    task.log(&format!("waiting for DSH host ({}/20s)", attempt / 4));
                }
            }
            let available = reqwest::blocking::get(&url)
                .map(|response| response.status().is_success())
                .unwrap_or(false);
            if !available {
                thread::sleep(Duration::from_millis(250));
            }
            available
        });
        if ready {
            // Record the host's descendant pids right after launch: the tsx
            // intermediate exits quickly and orphans the node host, so Stop
            // needs these pids to finish the tree kill.
            let tree = Arc::new(Mutex::new(Vec::new()));
            let collector_tree = tree.clone();
            let root_pid = child.id();
            std::thread::spawn(move || {
                collect_process_descendants(root_pid, collector_tree, Duration::from_secs(2));
            });
            manager
                .running
                .lock()
                .map_err(|_| "container manager lock failed")?
                .insert(
                    id,
                    ManagedHost {
                        child,
                        url: url.clone(),
                        tree,
                    },
                );
            if let Some(task) = task {
                task.update("DSH host is ready", 95);
            }
            return Ok(url);
        }
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            return Err("task cancelled while waiting for DSH host".to_owned());
        }
        let _ = kill_process_tree(child.id());
        let _ = child.wait();
        // A launch that dies with a pre-existing build usually means the
        // artifact is stale relative to the source; rebuild once and retry
        // before giving up. Already-built this round means there is nothing
        // more to try.
        if attempt == 1 && built {
            if let Some(task) = task {
                task.update("Rebuilding DSH frontend", 60);
                task.log("DSH launch failed; rebuilding frontend");
            }
            built = false;
            continue;
        }
        return Err(format!(
            "DSH host did not become ready; inspect {}",
            log_path.display()
        ));
    }
}

/// Terminate a process and its whole descendant tree. `Child::kill` only
/// stops the immediate process; a DSH host is a pnpm shell around node
/// servers, so on Windows we ask taskkill for the full tree — otherwise the
/// node processes keep listening on the port and the DSH frontend keeps
/// working after Stop.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("taskkill");
        suppress_console_window(&mut command);
        let _ = command
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Best effort on Unix: DSH Box launches no process groups, so fall
        // back to a plain kill and let the OS reparent the grandchildren.
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

/// Terminates every recorded descendant in one suppressed call where
/// possible, used to sweep the orphans re-parented when the pnpm launcher's
/// tsx layer exits. Sweeping one taskkill invocation per pid would flash a
/// console window each time.
pub(crate) fn kill_pids(pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("taskkill");
        suppress_console_window(&mut command);
        let mut args = vec!["/F".to_owned()];
        for pid in pids {
            args.push("/PID".to_owned());
            args.push(pid.to_string());
        }
        let _ = command.args(args).output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        for &pid in pids {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
}

/// Watches the process table for a short window and records every descendant
/// of `root` into `tree`. The pnpm launcher's tsx intermediate exits quickly,
/// orphaning the node host from the tree `taskkill /T` walks; recording the
/// actual pids lets Stop finish the job.
fn collect_process_descendants(root: u32, tree: Arc<Mutex<Vec<u32>>>, total: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < total {
        let procs = process_table();
        if let Ok(mut guard) = tree.lock() {
            let mut frontier = vec![root];
            while let Some(parent) = frontier.pop() {
                for &(pid, ppid) in &procs {
                    if pid != root && ppid == parent && !guard.contains(&pid) {
                        guard.push(pid);
                        frontier.push(pid);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Snapshot of (pid, parent pid) pairs for every process on the system.
#[cfg(target_os = "windows")]
fn process_table() -> Vec<(u32, u32)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut table = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return table;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                table.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    table
}

/// Snapshot of (pid, parent pid) pairs read from `/proc` (Linux). macOS has
/// no /proc, so the read fails and the table stays empty — a safe no-op that
/// keeps the old single-process kill behavior there.
#[cfg(not(target_os = "windows"))]
fn process_table() -> Vec<(u32, u32)> {
    let mut table = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // stat format: pid (comm) state ppid ...; comm may contain spaces.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let Some(ppid) = stat[close + 1..]
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        table.push((pid, ppid));
    }
    table
}

#[tauri::command]
pub(crate) fn stop_dsh_container(
    id: String,
    app: Option<&tauri::AppHandle>,
    manager: tauri::State<ContainerManager>,
) -> Result<(), String> {
    let host = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(&id);
    if let Some(mut host) = host {
        kill_process_tree(host.child.id());
        let _ = host.child.wait();
        // The tsx intermediate exits early and orphans the node host, so the
        // taskkill /T tree walk misses it; sweep the recorded descendants.
        let descendants = host
            .tree
            .lock()
            .map(|tree| tree.clone())
            .unwrap_or_default();
        kill_pids(&descendants);
    }
    // The DSH window stays interactive as long as it is alive, so close it
    // together with the host: a stopped container should not be usable.
    if let Some(app) = app {
        if let Some(window) = app.get_webview_window(&format!("dsh-front-{id}")) {
            let _ = window.close();
        }
    }
    Ok(())
}

/// Best-effort display name for a container window title, falling back to
/// the container id when the metadata cannot be read.
fn container_display_name(id: &str) -> String {
    let Ok(config) = read_config() else {
        return id.to_owned();
    };
    let Some(root) = config.runtime_directory else {
        return id.to_owned();
    };
    let Ok(metadata) = fs::read_to_string(container_directory(&root, id).join("container.json"))
    else {
        return id.to_owned();
    };
    serde_json::from_str::<serde_json::Value>(&metadata)
        .ok()
        .and_then(|value| value["name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| id.to_owned())
}

#[tauri::command]
// IMPORTANT: this command MUST be `async`. Building a webview window from
// a synchronous command handler deadlocks on Windows (wry#583): the native
// window gets created but `build()` never returns, leaving a blank window
// and silently skipping every statement after it. In an async command the
// body runs on the tokio runtime, where `build()` can safely block while
// the main event loop pumps the WebView2 controller creation.
pub(crate) async fn open_dsh_front(
    id: String,
    manager: tauri::State<'_, ContainerManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let url = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .get(&id)
        .map(|host| host.url.clone())
        .ok_or("DSH host is not running")?;
    write_startup_log(&format!("open_dsh_front called for {id}: {url}"));
    let label = format!("dsh-front-{id}");
    let window_title = format!("{} - DSH", container_display_name(&id));
    if let Some(window) = app.get_webview_window(&label) {
        write_startup_log("open_dsh_front: window exists, showing");
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        // A reused window may still show the previous host's URL: Stop closes
        // the window best-effort, and a close that lags (or is dropped) leaves
        // the stale page on screen while the old host process is already gone.
        // Force a navigation whenever the current URL differs, otherwise the
        // page keeps fetching the dead port and reports "Failed to fetch".
        let stale = window
            .url()
            .map(|current| current.as_str() != url.as_str())
            .unwrap_or(true);
        if stale {
            let target: tauri::Url = url
                .parse()
                .map_err(|error| format!("DSH front invalid url {url}: {error}"))?;
            let _ = window.navigate(target);
        }
        return Ok(());
    }
    let probe_app = app.clone();
    let probe_label = label.clone();
    let probe_url = url.clone();
    // Open the DSH host URL directly. IMPORTANT: do NOT set
    // additional_browser_args here — it forces a second WebView2
    // environment with different options but the same user data dir,
    // which leaves webviews in a broken state (blank window, navigation
    // never issued; tauri-apps/tauri#11144).
    let target: tauri::Url = url
        .parse()
        .map_err(|error| format!("DSH front invalid url {url}: {error}"))?;
    let window = WebviewWindowBuilder::new(
        &app,
        label,
        WebviewUrl::External(target),
    )
    .title(&window_title)
    .build()
    .map_err(|error| {
        write_startup_log(&format!("DSH front open failed: {error}"));
        format!("DSH front open failed: {error}")
    })?;
    write_startup_log("open_dsh_front: window built");
    // WebView2 stalls the initial navigation/rendering of a webview whose
    // window is not visible in the foreground (confirmed with a lab app:
    // the same window renders as soon as it is brought to the front and
    // stays blank while created in the background). Force the new window
    // to the front right after building it, then re-focus and re-trigger
    // the navigation from a background thread: the main window tends to
    // steal focus back immediately, and WebView2 only starts navigating
    // once the controller sees its window in the foreground.
    let _ = window.show();
    let _ = window.set_focus();
    let retry_app = app.clone();
    let retry_app_inner = app.clone();
    let retry_label = probe_label.clone();
    let retry_url = url.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        // Navigation calls must run on the main thread; hop back over.
        let _ = retry_app.run_on_main_thread(move || {
            if let Some(window) = retry_app_inner.get_webview_window(&retry_label) {
                let _ = window.show();
                let _ = window.set_focus();
                // Only re-trigger the navigation while the window has not
                // reached the target URL yet: a loaded page would just be
                // reloaded needlessly.
                let still_blank = window
                    .url()
                    .map(|current| current.as_str() != retry_url.as_str())
                    .unwrap_or(true);
                if still_blank {
                    if let Ok(target) = retry_url.parse() {
                        let _ = window.navigate(target);
                    }
                }
            }
        });
    });
    write_startup_log(&format!("DSH front opened: {url}"));
    // Diagnostics only: the page title probe is no longer used to open the
    // system browser automatically. WebView2 navigation is fixed by the
    // no-proxy + foreground focus handling above, and the DSH page keeps its
    // default title while the in-app notice modal is showing, so the probe
    // used to misjudge successful loads and pop a browser window on every
    // open. The manual open_dsh_front_browser command stays for fallback.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(15));
        let loaded = probe_app
            .get_webview_window(&probe_label)
            .map(|window| {
                window
                    .url()
                    .map(|current| current.as_str() == probe_url.as_str())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !loaded {
            write_startup_log(&format!(
                "DSH front did not reach {probe_url} after 15s"
            ));
        }
    });
    Ok(())
}

#[tauri::command]
pub(crate) fn open_dsh_front_browser(
    id: String,
    manager: tauri::State<ContainerManager>,
) -> Result<(), String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    let url = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .get(&id)
        .map(|host| host.url.clone())
        .ok_or("DSH host is not running")?;
    webbrowser::open(&url).map_err(|error| format!("cannot open system browser: {error}"))
}

pub(crate) fn rebuild_dsh_container_with_task(
    id: String,
    app: Option<&tauri::AppHandle>,
    manager: tauri::State<ContainerManager>,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    if let Some(task) = task {
        task.update("Stopping DSH host", 20);
    }
    stop_dsh_container(id.clone(), app, manager.clone())?;
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, &id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    let source = dsh_version_directory(
        &root,
        value["version"]
            .as_str()
            .ok_or("container has no version")?,
    );
    let pnpm = resolve_toolchain("pnpm")?;
    let log_path = directory.join("logs").join("rebuild.log");
    // Truncate the previous rebuild log; forwarding appends to it below.
    fs::File::create(&log_path).map_err(|error| format!("cannot create rebuild log: {error}"))?;
    let source_arg = source.to_string_lossy().into_owned();
    for (index, args) in [
        ["--dir", source_arg.as_ref(), "install"],
        ["--dir", source_arg.as_ref(), "build"],
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(task) = task {
            task.update(
                if index == 0 {
                    "Installing DSH dependencies"
                } else {
                    "Building DSH frontend"
                },
                if index == 0 { 45 } else { 70 },
            );
            task.check_cancelled()?;
        }
        let mut command = command_for_toolchain(&pnpm);
        command.args(args);
        let mut command = spawn_forwarding_log(&mut command, &log_path, task)
            .map_err(|error| format!("cannot run pnpm: {error}"))?;
        let status = wait_for_process(&mut command, task, "running pnpm")?;
        if !status.success() {
            return Err(format!(
                "DSH rebuild failed; inspect {}",
                log_path.display()
            ));
        }
    }
    start_dsh_container_with_task(id, manager, task).map(|_| ())
}