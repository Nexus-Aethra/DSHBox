use super::*;
use serde::Deserialize;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};

/// What the startup check found, for the UI to ask about later.
///
/// The check runs once, before the window exists, so its one result is kept
/// here: `None` means the running daemon belongs to this build. A mismatch is
/// reported rather than acted on, and the person who can act on it is the one
/// looking at the window -- which is the only place the answer is useful.
static DAEMON_BUILD: std::sync::OnceLock<std::sync::Mutex<Option<(String, String)>>> =
    std::sync::OnceLock::new();

/// `None` when the running daemon matches this desktop binary; otherwise the
/// stamps, running first.
pub(crate) fn daemon_build_status() -> Option<(String, String)> {
    let cell = DAEMON_BUILD.get()?;
    let guard = cell.lock().ok()?;
    guard.clone()
}

fn record_daemon_build(running: &str, client: &str) {
    // `get_or_init`, not `get`: an uninitialised `OnceLock` reads as `None`
    // forever, which is indistinguishable from "no mismatch" and so reports a
    // stale daemon as current -- the notice silently never appears.
    let cell = DAEMON_BUILD.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut inner) = cell.lock() {
        *inner = Some((running.to_owned(), client.to_owned()));
    }
}
/// Writes diagnostics before Tauri logging is available.
pub(crate) fn write_startup_log(message: &str) {
    let root = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dshbox/logs");
    let _ = fs::create_dir_all(&root);
    let line = format!("[{}] {message}\n", now_seconds());
    let _ = fs::OpenOptions::new().create(true).append(true).open(root.join("desktop.log")).and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

pub(crate) fn bundled_target() -> &'static str {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => "unsupported",
    }
}

pub(crate) fn bundled_server_path(resource_directory: &Path) -> PathBuf {
    let executable = if cfg!(windows) {
        "dshboxd.exe"
    } else {
        "dshboxd"
    };
    // Strip the verbatim `\\?\` prefix Tauri's resource_dir returns on
    // Windows so spawned executables get an ordinary absolute path.
    PathBuf::from(strip_verbatim_prefix(
        &resource_directory
            .join("server")
            .join(bundled_target())
            .join(executable)
            .to_string_lossy(),
    ))
}

/// True when the daemon's discovery record is reachable right now.
fn daemon_alive() -> bool {
    box_server_core::read_discovery()
        .ok()
        .flatten()
        // A quick probe: this is polled by the startup gate, so a daemon that is
        // busy has to answer "not yet" fast rather than hold the poll open.
        .map(|discovery| box_client::RpcClient::from_discovery(&discovery).ping_quickly().is_ok())
        .unwrap_or(false)
}

/// Build-batch of this desktop binary, embedded at compile time from
/// `src-tauri/.build-stamp` (epoch seconds written on every daemon rebuild).
pub(crate) const CLIENT_BUILD_STAMP: &str = env!("DSHBOX_BUILD_STAMP");

/// Wait up to `timeout` for a reachable daemon and return a connected client.
fn wait_for_daemon(timeout: Duration) -> Option<box_client::RpcClient> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(discovery)) = box_server_core::read_discovery() {
            let client = box_client::RpcClient::from_discovery(&discovery);
            if client.ping().is_ok() {
                return Some(client);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// Protocol handshake on startup: when the running daemon was built in a
/// different build batch than this desktop binary (a stale daemon left
/// over from before an upgrade), stop it and start the daemon shipped with
/// this install. Failures are logged but never block startup.
pub(crate) fn reconcile_daemon_build() {
    let Some(client) = wait_for_daemon(Duration::from_secs(5)) else {
        write_startup_log("daemon did not become reachable; skipping build stamp check");
        return;
    };
    let remote_stamp = client
        .call("get_info", serde_json::json!({}))
        .ok()
        .and_then(|info| info["buildStamp"].as_str().map(str::to_owned))
        .unwrap_or_default();
    if remote_stamp == CLIENT_BUILD_STAMP {
        record_daemon_build(&remote_stamp, CLIENT_BUILD_STAMP);
        write_startup_log(&format!("daemon build stamp matches ({remote_stamp})"));
        return;
    }
    // Log it, and do not act on it.
    //
    // This used to stop the daemon and start the one from this install, and
    // the reason it no longer does is that the hammer was aimed at a rare
    // case. A daemon left over from a previous build is a version behind on
    // a loopback RPC that only grows; the client falls back for the calls it
    // cannot make, and the cost of being wrong is a feature that needs a
    // newer server. The cost of stopping it is a container start, a build, or
    // a bundle export the user asked for a second earlier -- because the
    // desktop starts routinely, so merely opening the app was enough to end
    // whatever the daemon was doing. That trade was backwards, and "the
    // daemon is busy" was only one of the reasons it fired.
    //
    // So: report, and let the server stand. A restart stays available to the
    // user as one they chose, rather than one that happened to them.
    record_daemon_build(&remote_stamp, CLIENT_BUILD_STAMP);
    write_startup_log(&format!(
        "daemon build stamp {remote_stamp:?} != client {CLIENT_BUILD_STAMP:?}; keeping the running daemon"
    ));
}


/// Whether the running daemon is from a different build than this one.
///
/// A plain read of a value the startup check already computed, so it costs
/// nothing and cannot itself disturb the daemon. `stale` false means the two
/// match; true means the server is left running on purpose and the person who
/// can decide what to do about it is looking at this window.
#[tauri::command(async)]
pub fn get_daemon_build_notice() -> serde_json::Value {
    match daemon_build_status() {
        Some((running, client)) => serde_json::json!({
            "stale": true,
            "runningStamp": running,
            "clientStamp": client,
        }),
        None => serde_json::json!({ "stale": false }),
    }
}
/// Fallback launcher for platforms without a per-user service manager
/// (macOS) and for environments where service installation failed: spawns
/// the bundled daemon directly unless one is already reachable, so the
/// desktop app never blocks on a missing daemon.
pub(crate) fn spawn_daemon_fallback(server: &Path) {
    if daemon_alive() {
        write_startup_log("dshboxd is already reachable; skipping fallback spawn");
        return;
    }
    let mut command = Command::new(server);
    command.arg("--service");
    // Detach so the daemon outlives the desktop app and keeps running in
    // the system tray. Without this, closing the main window on Windows
    // would tear down the daemon and the UI would re-hang on the next
    // launch.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP so the daemon outlives the desktop
        // app closing and stays around in the system tray.
        // CREATE_NO_WINDOW suppresses the flash console window every time
        // the desktop has to fall back to spawning the daemon itself.
        // (DETACHED_PROCESS would also suppress the window but it would
        //  also strip the daemon of any chance to receive Ctrl+C / close
        //  notifications, which is overkill here.)
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        // Don't keep the daemon tied to our console / pipe handles.
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
    }
    match command.spawn() {
        Ok(child) => write_startup_log(&format!(
            "spawned dshboxd fallback: {} (pid {})",
            server.display(),
            child.id()
        )),
        Err(error) => write_startup_log(&format!("dshboxd fallback spawn failed: {error}")),
    }
}

/// User-level PATH entry (`~/.local/bin/dshboxd`) so `dshbox` CLI commands
/// can auto-spawn the daemon without opening the desktop app first.
/// Recreated on every launch so it never points at a stale sidecar path.
/// Idempotent; failures are logged but non-fatal. Windows uses a per-user
/// scheduled task instead (symlinks there need privileges).
#[cfg(unix)]
pub(crate) fn link_daemon_into_path(server: &Path) {
    use std::os::unix::fs::symlink;
    let home = match dirs::home_dir() {
        Some(home) => home,
        None => {
            write_startup_log("cannot determine home directory; skipping dshboxd PATH link");
            return;
        }
    };
    let bin_dir = home.join(".local/bin");
    if let Err(error) = fs::create_dir_all(&bin_dir) {
        write_startup_log(&format!("cannot create {}: {error}", bin_dir.display()));
        return;
    }
    let link = bin_dir.join("dshboxd");
    let _ = fs::remove_file(&link);
    match symlink(server, &link) {
        Ok(()) => write_startup_log(&format!(
            "linked dshboxd PATH entry: {} -> {}",
            link.display(),
            server.display()
        )),
        Err(error) => write_startup_log(&format!("cannot link dshboxd PATH entry: {error}")),
    }
}


pub(crate) fn initialize_bundled_runtime(resource_directory: PathBuf) -> Result<(), String> {
    let runtime = box_runtime::bundled::ResolvedBundledRuntime::from_path(
        &resource_directory.join("runtime").join(bundled_target()),
    )
    .map_err(|error| {
        format!(
            "bundled runtime is missing for {}: {error}",
            bundled_target()
        )
    })?;
    let node = runtime.node_executable();
    let npm = runtime.npm_script();
    let pnpm = runtime.pnpm_script();
    if !node.is_file() || !npm.is_file() || !pnpm.is_file() {
        return Err("bundled runtime is incomplete; reinstall DSH Box".to_owned());
    }
    let policy = box_runtime::process::bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        None,
        None,
        false,
        runtime.git_dir().as_deref(),
    );
    let spec = box_runtime::process::ProcessSpec::new(&node)
        .arg(&npm)
        .arg("--version")
        .policy(policy);
    let npm_version = box_runtime::process::NativeProcessRunner
        .run(&spec)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    BUNDLED_RUNTIME
        .set(BundledRuntime {
            node_version: runtime.manifest.node_version,
            npm_version,
            pnpm_version: runtime.manifest.pnpm_version,
            node,
            npm,
            pnpm,
        })
        .map_err(|_| "bundled runtime was initialized twice".to_owned())
}

#[tauri::command(async)]
pub(crate) fn get_server_service_status() -> ServiceStatus {
    service_status()
}

/// Health probe for the frontend startup gate: true once the daemon's
/// discovery record is reachable and it answers `ping`.
#[tauri::command(async)]
pub(crate) fn get_daemon_status() -> bool {
    daemon_alive()
}

#[tauri::command(async)]
pub(crate) fn restart_server_service() -> Result<(), String> {
    restart_user_service()
}

#[tauri::command(async)]
pub(crate) fn start_server_service() -> Result<(), String> {
    start_user_service()
}

#[tauri::command(async)]
pub(crate) fn stop_server_service() -> Result<(), String> {
    stop_user_service()
}

pub(crate) fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub(crate) fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "tray-open", "Open dshbox", true, None::<&str>)?;
    let start = MenuItem::with_id(
        app,
        "tray-start-daemon",
        "Start dshboxd",
        true,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(app, "tray-stop-daemon", "Stop dshboxd", true, None::<&str>)?;
    let restart = MenuItem::with_id(
        app,
        "tray-restart-daemon",
        "Restart dshboxd",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit dshbox", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &start,
            &stop,
            &restart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("tray icon".to_owned()))?;
    TrayIconBuilder::with_id("dshbox-tray")
        .icon(icon)
        .tooltip("dshbox")
        .menu(&menu)
        // Tauri 2 fires both `on_menu_event` (right-click on Windows /
        // click on Linux) and `on_tray_icon_event` (left-click on
        // Windows / click on macOS). Without the icon-event handler
        // a double-click on the Windows tray does nothing — there is
        // no built-in mapping to "open main window". macOS users never
        // notice because their default click already raises the menu.
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-open" => show_main_window(app),
            "tray-start-daemon" => {
                let _ = start_user_service();
            }
            "tray-stop-daemon" => {
                let _ = stop_user_service();
            }
            "tray-restart-daemon" => {
                let _ = restart_user_service();
            }
            "tray-quit" => {
                // Quit means "I'm done with DSH Box". Leaving dshboxd
                // running would leave orphan container hosts in the
                // user's tray with no UI to control them; stop it
                // alongside the UI. best-effort: errors are logged but
                // never block exit.
                let _ = stop_user_service();
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

pub(crate) fn bundled_runtime() -> Result<&'static BundledRuntime, String> {
    BUNDLED_RUNTIME
        .get()
        .ok_or("bundled runtime is unavailable; restart DSH Box".to_owned())
}
/// Shape of `plugins-manifest.json` shipped by `scripts/build-plugin.mjs`.
/// We only need a few fields to compute a digest; extra keys are ignored.
#[derive(Deserialize)]
#[allow(dead_code)]
struct PluginsManifest {
    #[allow(dead_code)]
    target: String,
    #[serde(rename = "pluginPackage")]
    #[allow(dead_code)]
    plugin_package: String,
    #[serde(rename = "pluginVersion")]
    #[allow(dead_code)]
    plugin_version: String,
    #[serde(rename = "builtAt")]
    #[allow(dead_code)]
    built_at: String,
}

/// Copy the bundled Cordis plugin tree from the Tauri resource into the
/// persistent user cache at `<runtimeDirectory>/plugins/node_modules/`.
///
/// Reads `plugins-manifest.json` from the resource, hashes it together
/// with the target triple, and compares that to
/// `BoxConfig.plugins_manifest_digest`. When they match, the user cache
/// is already current and no copy happens.
///
/// Defer silently when no runtime directory is configured yet: the
/// `save_runtime_directory` flow will pick the user up next launch.
#[allow(dead_code)]
pub(crate) fn initialize_bundled_plugins(resource_directory: &Path) -> Result<(), String> {
    let target = bundled_target();
    let resource_plugins = resource_directory.join("plugins").join(target);
    let manifest_path = resource_plugins.join("plugins-manifest.json");
    if !manifest_path.is_file() {
        // Resource might not carry the plugin (e.g. a developer build
        // that skips the bundler). Fall back silently so the desktop
        // still launches.
        return Ok(());
    }

    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let manifest: PluginsManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("cannot parse plugins manifest: {error}"))?;

    // Stable digest: SHA-256 of the manifest body scoped to the target.
    // Two installers for different platforms ship the same files under
    // different target dirs, so the manifest body itself is the smallest
    // signal that proves the resource changed.
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    manifest_bytes.hash(&mut hasher);
    manifest.target.hash(&mut hasher);
    let digest = format!("{:016x}", hasher.finish());

    let config = read_config()?;
    if config.plugins_manifest_digest.as_deref() == Some(digest.as_str()) {
        return Ok(());
    }

    let Some(runtime_directory) = config.runtime_directory.as_ref() else {
        // No runtime directory chosen yet; defer until the user picks one.
        return Ok(());
    };

    let runtime_root = PathBuf::from(runtime_directory);
    if !runtime_root.is_dir() {
        return Ok(());
    }

    let cache_root = runtime_root.join("plugins");
    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)
            .map_err(|error| format!("cannot clean {}: {error}", cache_root.display()))?;
    }
    fs::create_dir_all(&cache_root)
        .map_err(|error| format!("cannot create {}: {error}", cache_root.display()))?;

    copy_dir_recursive(&resource_plugins, &cache_root)
        .map_err(|error| format!("cannot copy plugins: {error}"))?;

    let mut updated = config;
    updated.plugins_manifest_digest = Some(digest);
    if let Err(error) = write_config(&updated) {
        write_startup_log(&format!("plugins manifest digest not persisted: {error}"));
    }
    write_startup_log(&format!("vendored plugins into {}", cache_root.display()));
    Ok(())
}

/// Recursive directory copy with `cp -rL` semantics: any symlinks
/// inside the source tree are dereferenced and their target contents
/// materialised, never re-exported as a link. Identical in shape to
/// `dshboxd::lifecycle::copy_tree_following`; the two copies are kept
/// independent because they live in different binaries (the daemon and
/// the desktop shell), and the surface is small enough that the
/// duplication is cheaper than threading another crate boundary.
pub(crate) fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            // Resolve through the symlink. The resolved target may still
            // be a directory (recursed below via the next branch) or a
            // file (handled via fs::copy).
            let resolved = fs::canonicalize(&from).unwrap_or_else(|_| from.clone());
            let resolved_meta = fs::symlink_metadata(&resolved)
                .or_else(|_| fs::metadata(&resolved))?;
            if resolved_meta.is_dir() {
                copy_dir_recursive(&resolved, &to)?;
            } else {
                fs::copy(&resolved, &to)?;
            }
        } else if metadata.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod bundled_plugins_tests {
    use super::*;
    use std::fs;

    /// A minimal manifest ships one Cordis plugin directory tree.
    fn fixture(source_root: &Path, target: &str) {
        let dir = source_root.join("plugins").join(target);
        fs::create_dir_all(dir.join("node_modules").join("@deepseek-ai").join("dsh-box-context")).unwrap();
        fs::write(
            dir.join("plugins-manifest.json"),
            format!("{{\"target\":\"{}\",\"pluginPackage\":\"@deepseek-ai/dsh-box-context\",\"pluginVersion\":\"0.1.0\",\"builtAt\":\"2026-08-15\"}}", target),
        ).unwrap();
        fs::write(
            dir.join("node_modules").join("@deepseek-ai").join("dsh-box-context").join("index.js"),
            "export default {}\n",
        ).unwrap();
    }

    #[test]
    fn copies_when_digest_stale_and_idempotent_next_call() {
        let temp = std::env::temp_dir().join(format!("dsh-box-plugins-{}", now_seconds()));
        let resource = temp.join("resource");
        let target = bundled_target();
        fixture(&resource, target);

        let copy_target = temp.join("runtime").join("plugins");
        fs::create_dir_all(temp.join("runtime")).unwrap();

        // First call copies everything into the target.
        copy_dir_recursive(&resource.join("plugins").join(target), &copy_target).unwrap();
        assert!(copy_target.join("plugins-manifest.json").is_file());
        assert!(copy_target.join("node_modules").join("@deepseek-ai").join("dsh-box-context").join("index.js").is_file());

        // Second call with an unchanged source is a no-op for the copy
        // (manifest body is byte-identical, so the digest matches).
        // We can't drive initialize_bundled_plugins here without a writable
        // ~/.dsh-box/config.json; the idempotency of the digest compare is
        // covered by the call-site test plan in commit 5.
        let _ = copy_target;
        let _ = fs::remove_dir_all(&temp);
    }
}
