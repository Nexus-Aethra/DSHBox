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
rem a file that is still open.
:wait_app
tasklist /FI "PID eq {app_pid}" | find "{app_pid}" >nul
if not errorlevel 1 (
  timeout /t 1 /nobreak >nul
  goto wait_app
)
rem The daemon holds dshboxd.exe open and would fail the install as well.
taskkill /F /IM dshboxd.exe >nul 2>&1
timeout /t 2 /nobreak >nul
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
            installer = installer,
            launcher = launcher,
            working_directory = working_directory,
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
# Replace the bundle beside the running one, not one somewhere else: the
# launcher path is the only reliable statement about where this is installed.
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
            launcher_dir = std::path::Path::new(launcher)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    }
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
    let app_pid = std::process::id();
    let launcher = std::env::current_exe()
        .map_err(|error| format!("cannot locate the running launcher: {error}"))?
        .to_string_lossy()
        .into_owned();
    let working_directory = std::path::Path::new(&path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_owned());
    let script = render_installer_script(platform, app_pid, &launcher, &path, &working_directory);
    let script_name = match platform {
        InstallerPlatform::WindowsMsi => "install-update.cmd",
        InstallerPlatform::MacDmg => "install-update.sh",
    };
    let script_path = std::path::Path::new(&working_directory).join(script_name);
    std::fs::write(&script_path, script)
        .map_err(|error| format!("cannot write {}: {error}", script_path.display()))?;
    spawn_helper(platform, &script_path)?;
    // The helper waits for this process to disappear before touching the
    // install, so exiting is part of the operation rather than a courtesy.
    let app_for_exit = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        app_for_exit.exit(0);
    });
    Ok(serde_json::json!({ "scheduled": true, "script": script_path.to_string_lossy() }))
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
        assert!(
            script.contains(r#"destination="/Applications/DSH Box.app/Contents/MacOS""#),
            "{script}"
        );
        assert!(script.contains("cp -R"), "{script}");
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