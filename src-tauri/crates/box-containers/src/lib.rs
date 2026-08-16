//! Persistent DSH container metadata independent from desktop windows.

use box_foundation::BoxResult;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshContainer {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Local template (or image alias) the container is based on; read from
    /// container.json so the UI and CLI can display/validate the binding.
    #[serde(default)]
    pub template: Option<String>,
    pub directory: String,
    pub status: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDshContainerRequest {
    pub name: String,
    pub version: String,
    pub profile: String,
}

fn default_profile() -> String {
    "web".to_owned()
}

pub fn container_directory(root: &str, id: &str) -> PathBuf {
    PathBuf::from(root).join("instances").join(id)
}

/// Path to the PID file dropped by the desktop app when a container host
/// is launched. The CLI (and any other read-only consumer) checks this file
/// plus a live `kill -0` / `tasklist` probe to decide whether the host is
/// still running, instead of trusting an in-memory map it cannot see.
pub fn host_pid_path(container: &DshContainer) -> PathBuf {
    PathBuf::from(&container.directory).join("state").join("host.pid")
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // Use kill -0 to probe the process without sending a signal. ESRCH
    // ("No such process") exits with status 1; a successful probe exits
    // with 0. We deliberately treat EPERM (also status 1) as "unknown"
    // rather than "alive" because the practical impact is identical: the
    // PID file is stale and dshbox ps reports "stopped". A subsequent
    // dshbox start recreates the file, so any false negative is
    // recoverable on the next user action.
    let output = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    // `tasklist` ships with every Windows install; `/NH` suppresses the
    // header so we only have to look for the PID literal in the body. Any
    // error (missing tool, denied permission, etc.) is treated as "unknown"
    // and the caller will fall back to "stopped".
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

/// Returns true when the host PID file exists and the process it points at
/// is still reachable. Used by `scan_containers` so that `dshbox ps`
/// and the desktop UI both see the same authoritative state.
pub fn is_host_alive(container: &DshContainer) -> bool {
    let pid_path = host_pid_path(container);
    let pid_text = match fs::read_to_string(&pid_path) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let Ok(pid) = pid_text.trim().parse::<u32>() else {
        return false;
    };
    pid_is_alive(pid)
}

pub fn scan_containers(root: &str) -> BoxResult<BTreeMap<String, DshContainer>> {
    let directory = PathBuf::from(root).join("instances");
    if !directory.exists() {
        return Ok(BTreeMap::new());
    }
    let mut containers = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
    {
        let metadata = match fs::read_to_string(entry.path().join("container.json")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let value: serde_json::Value = match serde_json::from_str(&metadata) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let (Some(id), Some(version)) = (value["id"].as_str(), value["version"].as_str()) else {
            continue;
        };
        let mut container = DshContainer {
            id: id.to_owned(),
            name: value["name"].as_str().unwrap_or(id).to_owned(),
            version: version.to_owned(),
            profile: value["profile"].as_str().unwrap_or("web").to_owned(),
            template: value["template"].as_str().map(str::to_owned),
            directory: entry.path().to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        };
        if is_host_alive(&container) {
            container.status = "running".to_owned();
        }
        containers.insert(id.to_owned(), container);
    }
    Ok(containers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    fn sandbox(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dshbox-containers-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn stub_container(directory: &Path, id: &str) -> DshContainer {
        let instance = directory.join("instances").join(id);
        fs::create_dir_all(instance.join("state")).unwrap();
        fs::write(
            instance.join("container.json"),
            format!(r#"{{"id":"{id}","name":"{id}","version":"latest","profile":"web"}}"#),
        )
        .unwrap();
        DshContainer {
            id: id.to_owned(),
            name: id.to_owned(),
            version: "latest".to_owned(),
            profile: "web".to_owned(),
            template: None,
            directory: instance.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        }
    }

    #[test]
    fn scan_containers_marks_running_when_pid_file_exists_and_process_is_alive() {
        let root = sandbox("alive");
        let container = stub_container(&root, "alive");
        // Spawn a long-lived child; we own the PID and the kernel keeps the
        // process alive while we hold it, so kill -0 is guaranteed to return
        // 0. The child is reaped at the end of the test.
        #[cfg(unix)]
        {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            fs::write(host_pid_path(&container), child.id().to_string()).unwrap();
            let containers = scan_containers(root.to_string_lossy().as_ref()).unwrap();
            let entry = containers.get("alive").expect("container present");
            assert_eq!(entry.status, "running");
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(not(unix))]
        {
            // Skip the live process probe on non-unix; the file-missing
            // branch is covered separately below.
            let _ = container;
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scan_containers_marks_stopped_when_pid_file_is_missing() {
        let root = sandbox("missing");
        let _ = stub_container(&root, "missing");
        let containers = scan_containers(root.to_string_lossy().as_ref()).unwrap();
        let entry = containers.get("missing").expect("container present");
        assert_eq!(entry.status, "stopped");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn scan_containers_marks_stopped_when_pid_file_points_at_dead_process() {
        // Spawn a short-lived child, wait it out, then point the PID file at
        // its (now-defunct) PID. We can only do this on unix because Windows
        // reuses PIDs aggressively; the file-missing branch above already
        // covers the practical "host crashed" scenario.
        let root = sandbox("dead");
        let container = stub_container(&root, "dead");
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let status = child.wait().unwrap();
        assert!(status.success());
        fs::write(host_pid_path(&container), pid.to_string()).unwrap();
        let containers = scan_containers(root.to_string_lossy().as_ref()).unwrap();
        let entry = containers.get("dead").expect("container present");
        assert_eq!(entry.status, "stopped");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scan_containers_marks_stopped_when_pid_file_is_malformed() {
        let root = sandbox("malformed");
        let container = stub_container(&root, "malformed");
        fs::write(host_pid_path(&container), "not-a-number").unwrap();
        let containers = scan_containers(root.to_string_lossy().as_ref()).unwrap();
        let entry = containers.get("malformed").expect("container present");
        assert_eq!(entry.status, "stopped");
        let _ = fs::remove_dir_all(root);
    }
}
