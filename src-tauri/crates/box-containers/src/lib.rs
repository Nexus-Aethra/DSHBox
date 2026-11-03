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
    /// Environment declared by the Boxfile's `ENV` directives, copied into
    /// container.json at creation and applied on top of the host spawn's
    /// environment whitelist.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Execution backend requested by the container record's optional
    /// `runtime` field. Absent means the host process backend; a VM request
    /// travels through this field and fails the start when the build cannot
    /// execute the named backend. Creation never writes the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
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

/// The name a container is shown by, read from its own record.
///
/// One record, not a scan: a caller that already has the canonical id needs the
/// display name to put in a message, and scanning every container to find one
/// would make an error path cost more than the successful path. A container
/// whose record is missing or unreadable answers `None`; the caller formats the
/// id instead, because a name it could not read is not a name it may invent.
pub fn container_name(root: &str, id: &str) -> Option<String> {
    let path = container_directory(root, id).join("container.json");
    let metadata = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(_) => return None,
    };
    let value: serde_json::Value = match serde_json::from_str(&metadata) {
        Ok(value) => value,
        Err(_) => return None,
    };
    value["name"].as_str().map(str::to_owned)
}

/// Path to the PID file dropped by the desktop app when a container host
/// is launched. The CLI (and any other read-only consumer) checks this file
/// plus a direct process query to decide whether the host is still running,
/// instead of trusting an in-memory map it cannot see.
pub fn host_pid_path(container: &DshContainer) -> PathBuf {
    PathBuf::from(&container.directory)
        .join("state")
        .join("host.pid")
}

/// True when the PID names a live process.
///
/// This delegates to `box_runtime::process::probe_pid`, which asks the OS
/// directly (a `/proc` read on Linux, an `OpenProcess`/`GetExitCodeProcess`
/// pair on Windows). The previous implementation shelled out to `tasklist`
/// on Windows, which cost ~450 ms per call, and this runs once per container
/// on every `list_containers` -- so opening a two-row page waited half a
/// second on process-spawning, for every row, before it could draw anything.
///
/// A PID we cannot query is reported as "not alive": `Recycled` and
/// `Unknown` both mean the host record cannot be trusted, and the caller
/// falls back to "stopped" -- which a later `dshbox start` repairs by
/// recreating the record.
fn pid_is_alive(pid: u32) -> bool {
    matches!(
        box_runtime::process::probe_pid(pid),
        box_runtime::process::PidState::Alive
    )
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

/// Check a persisted host PID when only the lifecycle record is available.
/// This is used after a daemon restart, when there is no in-memory child
/// handle but the host may still be serving its loopback URL.
pub fn is_host_pid_alive(pid: u32) -> bool {
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
            env: value["env"]
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            runtime: value["runtime"].as_str().map(str::to_owned),
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
            env: BTreeMap::new(),
            runtime: None,
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
