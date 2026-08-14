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
    pub endpoint: String,
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

pub fn default_endpoint() -> BoxResult<PathBuf> {
    Ok(server_directory()?.join("dshboxd.sock"))
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

pub fn write_discovery(endpoint: impl Into<String>) -> BoxResult<ServerDiscovery> {
    let directory = server_directory()?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let discovery = ServerDiscovery {
        token: uuid::Uuid::new_v4().to_string(),
        pid: process::id(),
        started_at: now_seconds(),
        endpoint: endpoint.into(),
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
        let running = Command::new("schtasks")
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
        fs::write(&unit, format!("[Unit]\nDescription=dshbox daemon\nAfter=default.target\n\n[Service]\nType=simple\nExecStart=\"{quoted}\" --service\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n")).map_err(|error| error.to_string())?;
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
        let output = Command::new("schtasks")
            .args([
                "/Create", "/TN", "dshboxd", "/TR", &task, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let _ = Command::new("schtasks")
            .args(["/Run", "/TN", "dshboxd"])
            .output();
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
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", "dshboxd"])
            .output();
        let output = Command::new("schtasks")
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
