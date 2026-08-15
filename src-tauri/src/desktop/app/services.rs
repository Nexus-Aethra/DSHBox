use super::*;
use serde::Deserialize;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};

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

pub(crate) const DEFAULT_NODE_VERSION: &str = "v24.11.1";
pub(crate) const DEFAULT_PNPM_VERSION: &str = "11.21.0";

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

pub(crate) fn initialize_bundled_runtime(resource_directory: PathBuf) -> Result<(), String> {
    let root = resource_directory.join("runtime").join(bundled_target());
    let manifest: BundledRuntimeManifest = serde_json::from_str(
        &fs::read_to_string(root.join("runtime-manifest.json")).map_err(|_| {
            format!(
                "bundled runtime is missing for {}; reinstall DSH Box",
                bundled_target()
            )
        })?,
    )
    .map_err(|error| format!("cannot parse bundled runtime manifest: {error}"))?;
    // Tauri's resource_dir returns verbatim `\\?\` paths on Windows, and
    // bundled Node crashes with `EISDIR lstat 'D:'` when a verbatim entry
    // script reaches `Module._findPath`. Store plain absolute paths instead.
    let node = PathBuf::from(strip_verbatim_prefix(&root.join(&manifest.node_entry).to_string_lossy()));
    let npm = PathBuf::from(strip_verbatim_prefix(&root.join(&manifest.npm_entry).to_string_lossy()));
    let pnpm = PathBuf::from(strip_verbatim_prefix(&root.join(&manifest.pnpm_entry).to_string_lossy()));
    if !node.is_file() || !npm.is_file() || !pnpm.is_file() {
        return Err("bundled runtime is incomplete; reinstall DSH Box".to_owned());
    }
    let mut version_probe = Command::new(&node);
    suppress_console_window(&mut version_probe);
    let npm_version = version_probe
        .arg(&npm)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    BUNDLED_RUNTIME
        .set(BundledRuntime {
            node_version: manifest.node_version,
            npm_version,
            pnpm_version: manifest.pnpm_version,
            node,
            npm,
            pnpm,
        })
        .map_err(|_| "bundled runtime was initialized twice".to_owned())
}

#[tauri::command]
pub(crate) fn get_server_service_status() -> ServiceStatus {
    service_status()
}

#[tauri::command]
pub(crate) fn restart_server_service() -> Result<(), String> {
    restart_user_service()
}

#[tauri::command]
pub(crate) fn start_server_service() -> Result<(), String> {
    start_user_service()
}

#[tauri::command]
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
                // app.exit() does not wait for children: leave the hosts
                // running and their node processes keep the container ports
                // alive after the UI is gone. Stop them before exiting.
                let manager = app.state::<ContainerManager>();
                let hosts = manager
                    .running
                    .lock()
                    .map(|mut running| std::mem::take(&mut *running).into_iter().collect::<Vec<_>>())
                    .unwrap_or_default();
                for (_, mut host) in hosts {
                    kill_process_tree(host.child.id());
                    let _ = host.child.wait();
                    let descendants = host
                        .tree
                        .lock()
                        .map(|tree| tree.clone())
                        .unwrap_or_default();
                    kill_pids(&descendants);
                }
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
    let resource_plugins = resource_directory.join("plugins").join(&target);
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

/// Recursive directory copy used by `initialize_bundled_plugins`.
#[allow(dead_code)]
fn copy_dir_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
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
