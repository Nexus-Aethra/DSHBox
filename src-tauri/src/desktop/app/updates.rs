//! Desktop half of the updater: the commands the Settings pane calls, and the
//! detached helper that performs the swap.
//!
//! The daemon decides whether an update may be applied and verifies the bytes.
//! This module does the part a process cannot do to itself: end the running
//! binaries and replace them. That work therefore happens after this process
//! is gone, in a helper that outlives it -- so the interesting part of this
//! file is a pure function that renders that helper script, kept unit-tested
//! because a broken script here is a failed install with no message.

use super::*;

fn forward(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let client = connect()?;
    call(&client, method, params)
}

#[tauri::command(async)]
pub(crate) fn get_update_state() -> Result<serde_json::Value, String> {
    forward("get_update_state", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn check_for_update() -> Result<serde_json::Value, String> {
    forward("check_update", serde_json::json!({}))
}

/// The version page's history list, forwarded to the daemon.
#[tauri::command(async)]
pub(crate) fn list_update_releases() -> Result<serde_json::Value, String> {
    forward("list_update_releases", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn set_update_policy(policy: String) -> Result<serde_json::Value, String> {
    forward("set_update_policy", serde_json::json!({ "policy": policy }))
}

/// Start the daemon's download task and return its record.
///
/// The download itself is the daemon's job (it owns the network and the file);
/// the desktop only enqueues it, and the pane polls the task for progress.
#[tauri::command(async)]
pub(crate) fn enqueue_update_download(version: Option<String>) -> Result<TaskRecord, String> {
    let value = forward(
        "enqueue_update_download",
        serde_json::json!({ "version": version }),
    )?;
    serde_json::from_value(value).map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command(async)]
pub(crate) fn skip_update_version(version: Option<String>) -> Result<serde_json::Value, String> {
    match version {
        Some(version) => forward("skip_update_version", serde_json::json!({ "version": version })),
        None => forward("skip_update_version", serde_json::json!({})),
    }
}

#[tauri::command(async)]
pub(crate) fn clear_update_skip() -> Result<serde_json::Value, String> {
    forward("clear_update_skip", serde_json::json!({}))
}

#[tauri::command(async)]
pub(crate) fn discard_update_download(version: Option<String>) -> Result<serde_json::Value, String> {
    match version {
        Some(version) => forward(
            "discard_update_download",
            serde_json::json!({ "version": version }),
        ),
        None => forward("discard_update_download", serde_json::json!({})),
    }
}

/// Open the folder the verified installer sits in.
#[tauri::command(async)]
pub(crate) fn open_update_folder(path: String) -> Result<(), String> {
    let target = std::path::Path::new(&path);
    let directory = target.parent().unwrap_or(target);
    if !directory.exists() {
        return Err(format!("{} does not exist", directory.display()));
    }
    webbrowser::open(&directory.to_string_lossy()).map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallerPlatform {
    WindowsMsi,
    MacDmg,
}

/// Which install helper this platform uses, if it has one.
///
/// Linux returns None on purpose: installing a .deb or .rpm needs root, and
/// escalating behind a GUI is not something to do on someone behalf. The pane
/// shows the command instead.
fn installer_platform(installer: &str) -> Option<InstallerPlatform> {
    match installer {
        "msi" => Some(InstallerPlatform::WindowsMsi),
        "dmg" => Some(InstallerPlatform::MacDmg),
        _ => None,
    }
}

/// The directory the new .app bundle belongs in: the parent of the running
/// bundle, found by walking up from the launcher to the component ending in
/// `.app`.
///
/// The launcher's own parent is `<bundle>.app/Contents/MacOS` -- *inside* the
/// old bundle. Copying there nests the new bundle inside the old one, and
/// `open "{launcher}"` then relaunches the old binary: an update that
/// reports success and updates nothing.
fn bundle_install_destination(launcher: &str) -> Option<String> {
    let bundle = std::path::Path::new(launcher)
        .ancestors()
        .find(|ancestor| ancestor.extension().and_then(|ext| ext.to_str()) == Some("app"))?;
    bundle
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
}

/// Escape a path for the cmd script: doubled quotes survive inside a quoted
/// argument, and a literal `%` must be doubled or cmd expands it.
fn cmd_quote(value: &str) -> String {
    value.replace('%', "%%").replace('"', "\"\"")
}

/// The helper script that installs one verified file, then relaunches this app.
///
/// Pure and parameterized on purpose: this is the one piece of the update path
/// that runs after the process which wrote it has died, so it has to be right
/// before it is ever used. The process id argument is this process, and the
/// script waits for it to disappear rather than sleeping a fixed amount -- a
/// fixed wait is either too short (the install fails on a locked file) or
/// wasted time.
///
/// Every path is interpolated inside quotes and never word-split, so an
/// install directory with a space in it (Program Files, a user folder) works.
fn render_installer_script(
    platform: InstallerPlatform,
    app_pid: u32,
    launcher: &str,
    installer: &str,
    working_directory: &str,
) -> String {
    match platform {
        InstallerPlatform::WindowsMsi => format!(
            r#"@echo off
setlocal
rem Written by DSH Box {version} to install a verified update.
rem Wait for the app that wrote this script to exit: the MSI cannot replace
rem a file that is still open. The sleeps are `ping`, not `timeout`: this
rem helper runs with redirected (nulled) stdin, and `timeout` refuses to wait
rem there -- which once turned this loop into a hot spin.
:wait_app
tasklist /FI "PID eq {app_pid}" | find "{app_pid}" >nul
if not errorlevel 1 (
  ping -n 2 127.0.0.1 >nul
  goto wait_app
)
rem The daemon holds dshboxd.exe open and would fail the install as well.
taskkill /F /IM dshboxd.exe >nul 2>&1
ping -n 3 127.0.0.1 >nul
start /wait "" msiexec /i "{installer}" /qn /norestart
set RESULT=%ERRORLEVEL%
if not "%RESULT%"=="0" if not "%RESULT%"=="3010" (
  echo DSH Box update failed with msiexec exit %RESULT% >"{working_directory}\update-error.log"
  exit /b %RESULT%
)
rem 3010 means succeeded, reboot required -- an update, not a failure.
start "" "{launcher}"
exit /b 0
"#,
            version = env!("CARGO_PKG_VERSION"),
            app_pid = app_pid,
            installer = cmd_quote(installer),
            launcher = cmd_quote(launcher),
            working_directory = cmd_quote(working_directory),
        ),
        InstallerPlatform::MacDmg => format!(
            r#"#!/bin/sh
# Written by DSH Box {version} to install a verified update.
# Wait for the app to exit before replacing it.
while kill -0 {app_pid} 2>/dev/null; do sleep 1; done
mount="$(mktemp -d)"
hdiutil attach "{installer}" -mountpoint "$mount" -nobrowse -quiet || exit 1
app="$(find "$mount" -maxdepth 1 -name '*.app' | head -1)"
if [ -z "$app" ]; then
  hdiutil detach "$mount" -quiet
  exit 1
fi
# Replace the bundle BESIDE the running one: destination is the directory
# containing the .app, not Contents/MacOS inside it -- nesting the new bundle
# inside the old one relaunches the old binary and calls it success.
destination="{launcher_dir}"
rm -rf "$destination/$(basename "$app")"
cp -R "$app" "$destination/"
hdiutil detach "$mount" -quiet
rmdir "$mount" 2>/dev/null
open "{launcher}"
"#,
            version = env!("CARGO_PKG_VERSION"),
            app_pid = app_pid,
            installer = installer,
            launcher = launcher,
            launcher_dir = bundle_install_destination(launcher).unwrap_or_else(|| {
                // No .app ancestor (a dev run from target/release): keep the
                // old behaviour rather than writing a broken script.
                std::path::Path::new(launcher)
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
        ),
    }
}

/// One staged update per process. The pane's Install button and the quit path
/// both land here; two spawns would mean two installers racing one file.
static INSTALL_STAGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Validate, render, and spawn the install helper for one verified file.
///
/// Shared by the pane's Install button and the quit path -- the same
/// operation, differing only in who schedules this process's exit afterwards.
fn stage_install(platform: InstallerPlatform, path: &str) -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::Ordering;
    if INSTALL_STAGED.swap(true, Ordering::SeqCst) {
        return Err("an update is already staged in this session".to_owned());
    }
    let staged = (|| {
        let app_pid = std::process::id();
        let launcher = std::env::current_exe()
            .map_err(|error| format!("cannot locate the running launcher: {error}"))?
            .to_string_lossy()
            .into_owned();
        let working_directory = std::path::Path::new(path)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_owned());
        let script = render_installer_script(platform, app_pid, &launcher, path, &working_directory);
        let script_name = match platform {
            InstallerPlatform::WindowsMsi => "install-update.cmd",
            InstallerPlatform::MacDmg => "install-update.sh",
        };
        let script_path = std::path::Path::new(&working_directory).join(script_name);
        std::fs::write(&script_path, script)
            .map_err(|error| format!("cannot write {}: {error}", script_path.display()))?;
        spawn_helper(platform, &script_path)?;
        Ok(script_path)
    })();
    // A failed staging must not brick the next attempt.
    if staged.is_err() {
        INSTALL_STAGED.store(false, Ordering::SeqCst);
    }
    staged
}

/// Stage the verified installer and start the helper that applies it.
///
/// Answers before the app exits, because the caller is the app: the UI needs a
/// moment to say what is about to happen.
#[tauri::command(async)]
pub(crate) fn install_update(app: tauri::AppHandle, path: String) -> Result<serde_json::Value, String> {
    let state = forward("get_update_state", serde_json::json!({}))?;
    let installer = state["available"]["asset"]["installer"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let Some(platform) = installer_platform(&installer) else {
        return Err(format!(
            "this platform installs {installer} packages by hand: run your package manager on {path}"
        ));
    };
    if !std::path::Path::new(&path).exists() {
        return Err(format!("the verified installer {path} is missing"));
    }
    // The webview names the file; the daemon owns the fact. Without this
    // check, `install update` is `msiexec /qn` on whatever a caller says.
    let verified = state["download"]["path"].as_str().unwrap_or("");
    if verified.is_empty() || std::path::Path::new(verified) != std::path::Path::new(&path) {
        return Err(
            "the installer must be the file the daemon verified -- check for updates and download again"
                .to_owned(),
        );
    }
    let script_path = stage_install(platform, &path)?;
    // The helper waits for this process to disappear before touching the
    // install, so exiting is part of the operation rather than a courtesy.
    let app_for_exit = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        app_for_exit.exit(0);
    });
    Ok(serde_json::json!({ "scheduled": true, "script": script_path.to_string_lossy() }))
}

/// Stage the automatic update when quitting with one ready. Best effort in
/// both directions: a daemon that cannot answer, no candidate, or a staging
/// failure all mean "just quit" -- quitting must never acquire a failure mode
/// it did not have before. Runs BEFORE the daemon is stopped: the state it
/// reads is the daemon's, and the helper it spawns waits for this process to
/// die from the outside.
///
/// The candidate test (`install_on_quit_candidate`) owns the rules: policy
/// auto, something newer, a verified file, an unattended installer. This
/// function is only the wiring that makes those rules run at quit time.
/// The quit path never spawns a daemon, and never calls \`connect()\`: its
/// fallback spawns one and polls for seconds, which would turn quitting into
/// starting. Read the discovery file directly and take only a
/// demonstrably-alive daemon.
fn quit_state() -> Option<serde_json::Value> {
    let discovery = box_server_core::read_discovery().ok().flatten()?;
    let client = box_client::RpcClient::from_discovery(&discovery);
    client.ping_quickly().ok()?;
    call(&client, "get_update_state", serde_json::json!({})).ok()
}

pub(crate) fn install_on_quit_if_any() {
    use std::sync::atomic::Ordering;
    // One attempt per process: the tray path and the runtime's
    // exit-requested event (the macOS menu quit) both land here, and the
    // second call must not re-read daemon state -- possibly from a daemon
    // that is already stopping -- on the way out.
    static ATTEMPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ATTEMPTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Both call sites run on the window's main thread, so the work moves to
    // a helper bounded by a short join: a healthy daemon answers a record
    // read in milliseconds, and a wedged one costs the quit three seconds,
    // not its RPC timeout. The thread uses quit_state() -- no spawn fallback
    // -- so even one that outlives the wait cannot bring a daemon back while
    // the user is leaving. One that finishes its staging detached is exactly
    // the design the helper was built for: the script waits for this process
    // to die, then installs.
    let (done, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Some(state) = quit_state() {
            if let Some(candidate) = install_on_quit_candidate(&state) {
                if let Some(path) = candidate["path"].as_str() {
                    let installer = candidate["installer"].as_str().unwrap_or("");
                    if let Some(platform) = installer_platform(installer) {
                        if std::path::Path::new(path).exists() {
                            if let Err(error) = stage_install(platform, path) {
                                // Surface, do not panic: the user is quitting
                                // either way, but the next diagnosis should
                                // not start from zero.
                                write_startup_log(&format!("update-on-quit not staged: {error}"));
                            }
                        }
                    }
                }
            }
        }
        let _ = done.send(());
    });
    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(3));
}

/// Start the helper detached, so it survives this process exiting.
fn spawn_helper(platform: InstallerPlatform, script: &std::path::Path) -> Result<(), String> {
    let mut command = match platform {
        InstallerPlatform::WindowsMsi => {
            let mut command = std::process::Command::new("cmd");
            command.arg("/c").arg(script);
            command
        }
        InstallerPlatform::MacDmg => {
            let mut command = std::process::Command::new("sh");
            command.arg(script);
            command
        }
    };
    // A child inherits the parent process group on unix, which is exactly how
    // the helper would be killed along with the app it is replacing.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start the install helper: {error}"))?;
    Ok(())
}

/// The update waiting to be applied when the app quits, or null.
///
/// Read through the daemon so the desktop and the daemon cannot disagree about
/// whether a verified file exists: the daemon owns that file.
#[tauri::command(async)]
pub(crate) fn get_install_on_quit() -> Result<serde_json::Value, String> {
    let client = connect()?;
    let state = call(&client, "get_update_state", serde_json::json!({}))?;
    Ok(serde_json::json!({
        "candidate": install_on_quit_candidate(&state),
        "policy": state["policy"],
    }))
}

/// Which update, if any, may be installed when this app quits.
///
/// Three conditions, all of them necessary: the policy has to say so, a
/// verified file has to be present, and the platform has to have an
/// unattended installer. Anything else is an update the user has to apply.
fn install_on_quit_candidate(state: &serde_json::Value) -> Option<serde_json::Value> {
    if state["policy"].as_str() != Some("auto") {
        return None;
    }
    if state["available"]["newer"].as_bool() != Some(true) {
        return None;
    }
    let path = state["download"]["path"].as_str()?;
    // The installer kind comes from the release manifest the daemon read. An
    // absent asset is not an automatic update: there would be no digest to
    // have verified the file with.
    let installer = state["available"]["asset"]["installer"].as_str()?;
    installer_platform(installer)?;
    if state["available"]["manualReason"].as_str().is_some() {
        return None;
    }
    Some(serde_json::json!({
        "version": state["available"]["version"],
        "path": path,
        "installer": installer,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_helper_waits_for_this_process_then_installs_and_relaunches() {
        let script = render_installer_script(
            InstallerPlatform::WindowsMsi,
            4242,
            r#"C:\Program Files\DSH Box\dshbox.exe"#,
            r#"D:\dshbox-data\updates\0.2.0\dshbox.msi"#,
            r#"D:\dshbox-data\updates\0.2.0"#,
        );
        // Waiting on the process id, not a fixed sleep: a fixed wait either
        // fails the install on a locked file or wastes time.
        assert!(script.contains(r#"tasklist /FI "PID eq 4242""#), "{script}");
        assert!(script.contains("goto wait_app"), "{script}");
        // `timeout` refuses to wait with redirected stdin; `ping` does.
        assert!(script.contains("ping -n 2 127.0.0.1 >nul"), "{script}");
        assert!(script.contains("ping -n 3 127.0.0.1 >nul"), "{script}");
        assert!(!script.contains("timeout /t"), "{script}");
        // The daemon holds its own executable open and would fail the MSI.
        assert!(script.contains("taskkill /F /IM dshboxd.exe"), "{script}");
        assert!(
            script.contains(r#"msiexec /i "D:\dshbox-data\updates\0.2.0\dshbox.msi" /qn /norestart"#),
            "{script}"
        );
        assert!(
            script.contains(r#"start "" "C:\Program Files\DSH Box\dshbox.exe""#),
            "{script}"
        );
        // 3010 is success-with-reboot, not a failure.
        assert!(script.contains(r#"if not "%RESULT%"=="3010""#), "{script}");
    }

    #[test]
    fn the_macos_helper_replaces_the_bundle_beside_the_running_one() {
        let script = render_installer_script(
            InstallerPlatform::MacDmg,
            4242,
            "/Applications/DSH Box.app/Contents/MacOS/dshbox",
            "/tmp/dshbox_0.2.0_aarch64.dmg",
            "/tmp",
        );
        assert!(script.contains("while kill -0 4242"), "{script}");
        assert!(script.contains("hdiutil attach"), "{script}");
        assert!(script.contains("hdiutil detach"), "{script}");
        // The destination is the directory CONTAINING the bundle: copying
        // into Contents/MacOS nests the new bundle inside the old one and
        // relaunches the old binary -- an update that reports success and
        // updates nothing.
        assert!(script.contains("destination=\"/Applications\""), "{script}");
        assert!(!script.contains("destination=\"/Applications/DSH Box.app\""), "{script}");
        assert!(script.contains("cp -R"), "{script}");
    }

    #[test]
    fn the_bundle_destination_is_the_app_bundle_parent() {
        assert_eq!(
            bundle_install_destination("/Applications/DSH Box.app/Contents/MacOS/dshbox")
                .as_deref(),
            Some("/Applications")
        );
        // A launcher with no .app ancestor has no bundle to walk out of.
        assert_eq!(bundle_install_destination("/usr/local/bin/dshbox"), None);
    }

    #[test]
    fn windows_paths_are_quoted_for_cmd() {
        // Quotes double inside a quoted argument; a literal % must double or
        // cmd expands it as a variable.
        assert_eq!(cmd_quote("C:\\Program Files\\a\"b"), "C:\\Program Files\\a\"\"b");
        assert_eq!(cmd_quote("D:\\x%y\\z.msi"), "D:\\x%%y\\z.msi");
    }

    #[test]
    fn a_missing_installer_path_is_not_a_silent_success() {
        // The failure mode this rules out: an update that reports "scheduled"
        // and then does nothing because the file it names is gone.
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": null,
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_needs_the_auto_policy() {
        let state = serde_json::json!({
            "policy": "download",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": { "path": "C:\\x\\dshbox.msi" },
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_is_refused_where_no_unattended_installer_exists() {
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "deb" } },
            "download": { "path": "/tmp/dshbox.deb" },
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_accepts_a_ready_automatic_update() {
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": { "path": "C:\\x\\dshbox.msi" },
        });
        let candidate = install_on_quit_candidate(&state).expect("a candidate");
        assert_eq!(candidate["version"], "0.2.0");
    }

    #[test]
    fn linux_installs_are_refused_rather_than_faked() {
        // A deb needs root; the pane shows the command instead of pretending.
        assert!(installer_platform("deb").is_none());
        assert!(installer_platform("rpm").is_none());
        assert!(installer_platform("msi").is_some());
        assert!(installer_platform("dmg").is_some());
    }
}