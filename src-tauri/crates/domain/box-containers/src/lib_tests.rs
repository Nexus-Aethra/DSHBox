//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

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
