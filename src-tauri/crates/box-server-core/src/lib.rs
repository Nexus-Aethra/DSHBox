//! Local single-user control plane shared by the DSH Box desktop app and server.

use box_foundation::{config_path, now_seconds, BoxResult};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    process::{self, Command},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDiscovery {
    pub token: String,
    pub pid: u32,
    pub started_at: u64,
    #[serde(default)]
    pub port: u16,
    /// Unix domain socket path. Legacy field from older daemon builds;
    /// kept for backward-compatible deserialization of stale discovery files.
    #[serde(default, skip_serializing)]
    pub endpoint: Option<String>,
}

impl Default for ServerDiscovery {
    fn default() -> Self {
        Self {
            token: String::new(),
            pid: 0,
            started_at: 0,
            port: 0,
            endpoint: None,
        }
    }
}

pub fn server_directory() -> BoxResult<PathBuf> {
    config_path()?
        .parent()
        .map(|path| path.join("server"))
        .ok_or_else(|| "invalid DSH Box configuration path".to_owned())
}

pub fn discovery_path() -> BoxResult<PathBuf> {
    Ok(server_directory()?.join("discovery.json"))
}

pub fn read_discovery() -> BoxResult<Option<ServerDiscovery>> {
    let path = discovery_path()?;
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_str(&fs::read_to_string(path).map_err(|error| error.to_string())?)
        .map(Some)
        .map_err(|error| error.to_string())
}

pub fn write_discovery(port: u16) -> BoxResult<ServerDiscovery> {
    let directory = server_directory()?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let discovery = ServerDiscovery {
        token: uuid::Uuid::new_v4().to_string(),
        pid: process::id(),
        started_at: now_seconds(),
        port,
        endpoint: None,
    };
    let temporary = directory.join("discovery.json.tmp");
    fs::write(
        &temporary,
        serde_json::to_string_pretty(&discovery).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, discovery_path()?).map_err(|error| error.to_string())?;
    Ok(discovery)
}

pub fn remove_discovery() {
    if let Ok(path) = discovery_path() {
        let _ = fs::remove_file(path);
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub supported: bool,
    pub enabled: bool,
    pub running: bool,
    pub detail: String,
}

pub fn service_status() -> ServiceStatus {
    #[cfg(target_os = "linux")]
    {
        let enabled = Command::new("systemctl")
            .args(["--user", "is-enabled", "dshboxd.service"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        let running = Command::new("systemctl")
            .args(["--user", "is-active", "dshboxd.service"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        return ServiceStatus {
            supported: true,
            enabled,
            running,
            detail: "systemd user service".to_owned(),
        };
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("schtasks");
        box_foundation::suppress_console_window(&mut command);
        let running = command
            .args(["/Query", "/TN", "dshboxd", "/FO", "LIST"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        return ServiceStatus {
            supported: true,
            enabled: running,
            running: read_discovery().ok().flatten().is_some(),
            detail: "Windows Task Scheduler".to_owned(),
        };
    }
    #[allow(unreachable_code)]
    ServiceStatus {
        supported: false,
        enabled: false,
        running: false,
        detail: "background service is not configured for this platform".to_owned(),
    }
}

pub fn install_user_service(executable: &std::path::Path) -> BoxResult<()> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or("cannot determine home directory")?;
        let unit = home.join(".config/systemd/user/dshboxd.service");
        fs::create_dir_all(unit.parent().ok_or("invalid systemd unit path")?)
            .map_err(|error| error.to_string())?;
        let quoted = executable.to_string_lossy().replace('"', "\\\"");
        fs::write(&unit, format!("[Unit]\nDescription=dshbox daemon\nAfter=default.target\nStartLimitIntervalSec=60\nStartLimitBurst=5\n\n[Service]\nType=simple\nExecStart=\"{quoted}\" --service\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n")).map_err(|error| error.to_string())?;
        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "--now", "dshboxd.service"],
        ] {
            let output = Command::new("systemctl")
                .args(args)
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let task = format!("\"{}\" --service", executable.display());
        let mut create = Command::new("schtasks");
        box_foundation::suppress_console_window(&mut create);
        let output = create
            .args([
                "/Create", "/TN", "dshboxd", "/TR", &task, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let mut run = Command::new("schtasks");
        box_foundation::suppress_console_window(&mut run);
        let _ = run.args(["/Run", "/TN", "dshboxd"]).output();
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("background service is not supported for this platform".to_owned())
}

pub fn restart_user_service() -> BoxResult<()> {
    #[cfg(target_os = "linux")]
    {
        let output = Command::new("systemctl")
            .args(["--user", "restart", "dshboxd.service"])
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    #[cfg(target_os = "windows")]
    {
        let mut end = Command::new("schtasks");
        box_foundation::suppress_console_window(&mut end);
        let _ = end.args(["/End", "/TN", "dshboxd"]).output();
        let mut run = Command::new("schtasks");
        box_foundation::suppress_console_window(&mut run);
        let output = run
            .args(["/Run", "/TN", "dshboxd"])
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    #[allow(unreachable_code)]
    Err("background service is not supported for this platform".to_owned())
}

/// Starts the per-user daemon without opening a desktop window.
pub fn start_user_service() -> BoxResult<()> {
    #[cfg(target_os = "linux")]
    return run_systemctl(&["--user", "start", "dshboxd.service"]);
    #[cfg(target_os = "windows")]
    return run_schtasks(&["/Run", "/TN", "dshboxd"]);
    #[allow(unreachable_code)]
    Err("background service is not supported for this platform".to_owned())
}

/// Stops the per-user daemon while keeping the desktop tray application open.
pub fn stop_user_service() -> BoxResult<()> {
    #[cfg(target_os = "linux")]
    return run_systemctl(&["--user", "stop", "dshboxd.service"]);
    #[cfg(target_os = "windows")]
    {
        // schtasks /End only knows about the daemon that the scheduled
        // task itself spawned. When the desktop fell back to launching
        // dshboxd directly (because schtasks /Run was rejected), there
        // is no scheduled task for /End to stop — the live dshboxd is
        // an orphan process the desktop spawned itself. So we try, in
        // order:
        //   1. graceful RPC shutdown via the discovery record (token-
        //      checked, daemon writes its final state and reaps hosts);
        //   2. wait up to 3s for the daemon to actually exit;
        //   3. taskkill /F /T /PID as the last resort;
        //   4. schtasks /End so the scheduled task itself doesn't
        //      immediately respawn the daemon on the next login.
        let mut stopped = false;
        let mut last_error: Option<String> = None;

        if let Ok(Some(discovery)) = read_discovery() {
            if graceful_shutdown_via_rpc(&discovery).is_ok() {
                for _ in 0..30 {
                    if !pid_alive(discovery.pid) {
                        stopped = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            if !stopped && pid_alive(discovery.pid) {
                let mut kill = std::process::Command::new("taskkill");
                box_foundation::suppress_console_window(&mut kill);
                match kill.args(["/F", "/T", "/PID", &discovery.pid.to_string()]).output() {
                    Ok(out) if out.status.success() => stopped = true,
                    Ok(out) => last_error = Some(String::from_utf8_lossy(&out.stderr).into_owned()),
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
        }

        if let Err(error) = run_schtasks(&["/End", "/TN", "dshboxd"]) {
            if !stopped {
                last_error.get_or_insert(error);
            }
        }

        return if stopped {
            Ok(())
        } else {
            Err(last_error.unwrap_or_else(|| "dshboxd is not running".to_owned()))
        };
    }
    #[allow(unreachable_code)]
    Err("background service is not supported for this platform".to_owned())
}

/// Windows-only: returns true when a process with the given PID still exists.
/// Used as the fallback path of `stop_user_service` to confirm the daemon
/// really went away after we asked it to shut down.
#[cfg(target_os = "windows")]
fn pid_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(&format!(",{pid},"))
        }
        Err(_) => false,
    }
}

/// Windows-only: open a single TCP connection to the daemon's discovery
/// port, POST `{"method":"shutdown","token":"..."}`, and return Ok(()) if
/// the daemon answered 200. We can't pull in box-client because it
/// already depends on box-server-core, so we hand-roll the request here.
#[cfg(target_os = "windows")]
fn graceful_shutdown_via_rpc(discovery: &ServerDiscovery) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = format!("127.0.0.1:{}", discovery.port);
    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|error| format!("bad discovery address: {error}"))?,
        Duration::from_millis(500),
    )
    .map_err(|error| format!("connect dshboxd {addr}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|error| format!("set read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|error| format!("set write timeout: {error}"))?;

    let body = serde_json::json!({
        "token": discovery.token,
        "method": "shutdown",
    })
    .to_string();
    let request = format!(
        "POST /rpc HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write request: {error}"))?;
    stream.flush().map_err(|error| format!("flush: {error}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read response: {error}"))?;
    if response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "dshboxd rejected shutdown: {}",
            response.lines().next().unwrap_or("")
        ))
    }
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> BoxResult<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

#[cfg(target_os = "windows")]
fn run_schtasks(args: &[&str]) -> BoxResult<()> {
    let mut command = Command::new("schtasks");
    box_foundation::suppress_console_window(&mut command);
    let output = command
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Registers the UI in the graphical user's login session so its tray menu is available.
pub fn install_tray_autostart(executable: &std::path::Path) -> BoxResult<()> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or("cannot determine home directory")?;
        let entry = home.join(".config/autostart/dshbox-tray.desktop");
        fs::create_dir_all(entry.parent().ok_or("invalid autostart path")?)
            .map_err(|error| error.to_string())?;
        let quoted = executable.to_string_lossy().replace('"', "\\\"");
        fs::write(&entry, format!("[Desktop Entry]\nType=Application\nName=dshbox\nComment=DSH Box tray controls\nExec=\"{quoted}\" --tray\nTerminal=false\nX-GNOME-Autostart-enabled=true\n")).map_err(|error| error.to_string())?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let command = format!("\"{}\" --tray", executable.display());
        return run_schtasks(&[
            "/Create",
            "/TN",
            "dshbox-tray",
            "/TR",
            &command,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ]);
    }
    #[allow(unreachable_code)]
    Ok(())
}