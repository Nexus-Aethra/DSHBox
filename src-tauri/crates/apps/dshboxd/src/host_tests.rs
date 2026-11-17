//! tests for host.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    fn snapshot(id: &str, gen: u64) -> ContainerHostRecord {
        ContainerHostRecord {
            id: id.to_owned(),
            name: "test".to_owned(),
            template: Some("tmpl".to_owned()),
            profile: "web".to_owned(),
            host_pid: 1234,
            host_pgid: 1234,
            host_port: 40000,
            host_url: "http://127.0.0.1:40000".to_owned(),
            authenticated_url: None,
            started_at: 1,
            last_seen: 1,
            state: HostState::Running,
            generation: gen,
            exit_status: None,
            exit_signal: None,
            unhealthy_count: 0,
            probe_count: 0,
        }
    }

    #[test]
    fn authenticated_url_round_trips_and_defaults_to_none() {
        let mut snap = snapshot("c-auth", 1);
        snap.authenticated_url =
            Some("http://127.0.0.1:40000/?token=abc".to_owned());
        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains("authenticatedUrl"));
        let parsed: ContainerHostRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.authenticated_url.as_deref(), Some("http://127.0.0.1:40000/?token=abc"));

        // A legacy record written before the field existed parses fine.
        let mut legacy_value: serde_json::Value = serde_json::from_str(&text).unwrap();
        legacy_value.as_object_mut().unwrap().remove("authenticatedUrl").unwrap();
        let parsed_legacy: ContainerHostRecord =
            serde_json::from_value(legacy_value).unwrap();
        assert_eq!(parsed_legacy.authenticated_url, None);
    }

    #[test]
    fn exit_status_to_parts_handles_success() {
        let status = exit_status_command(0)
            .status()
            .expect("spawn successful command");
        let (code, _signal) = exit_status_to_parts(status);
        assert_eq!(code, 0);
        #[cfg(unix)]
        assert_eq!(_signal, None);
    }

    #[test]
    fn exit_status_to_parts_handles_failure() {
        let status = exit_status_command(1)
            .status()
            .expect("spawn failing command");
        let (code, _signal) = exit_status_to_parts(status);
        assert_eq!(code, 1);
    }

    #[test]
    fn describe_exit_combines_status_and_signal() {
        assert_eq!(describe_exit(Some(0), None), "exit code 0");
        #[cfg(unix)]
        assert_eq!(describe_exit(None, Some(11)), "signal 11");
        assert_eq!(describe_exit(None, None), "unknown exit");
    }

    fn exit_status_command(code: i32) -> std::process::Command {
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("cmd.exe");
            command.args(["/C", "exit", &code.to_string()]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", &format!("exit {code}")]);
            command
        }
    }

    // CAS tests need a writable runtime dir; they call the `_in`
    // variants directly with a tempdir rather than rewriting
    // `~/.dsh-box/config.json`.
    #[test]
    fn cas_bumps_generation_and_persists() {
        let runtime_root = tempdir_runtime_root();
        std::fs::create_dir_all(host_record_path(&runtime_root, "c-1").parent().unwrap()).unwrap();
        let mut snap = snapshot("c-1", 1);
        snap.state = HostState::Starting;
        write_host_record_in(&runtime_root, &snap).unwrap();

        let outcome = compare_and_swap_host_record_in(&runtime_root, "c-1", &snap, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Ready;
            next
        })
        .unwrap();
        match outcome {
            CasOutcome::Applied(next) => {
                assert_eq!(next.generation, 2);
                assert_eq!(next.state, HostState::Ready);
            }
            CasOutcome::Stale { .. } => panic!("CAS should not be stale"),
        }
        let reread = read_host_record_in(&runtime_root, "c-1").unwrap().unwrap();
        assert_eq!(reread.generation, 2);
        assert_eq!(reread.state, HostState::Ready);
    }

    #[test]
    fn cas_returns_stale_when_generation_moves() {
        let runtime_root = tempdir_runtime_root();
        std::fs::create_dir_all(host_record_path(&runtime_root, "c-2").parent().unwrap()).unwrap();
        let snap = snapshot("c-2", 1);
        write_host_record_in(&runtime_root, &snap).unwrap();
        // Simulate a concurrent writer bumping generation to 5.
        let mut fresh = snap.clone();
        fresh.generation = 5;
        fresh.state = HostState::Stopped;
        write_host_record_in(&runtime_root, &fresh).unwrap();

        let outcome = compare_and_swap_host_record_in(&runtime_root, "c-2", &snap, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Crashed;
            next
        })
        .unwrap();
        match outcome {
            CasOutcome::Stale { on_disk } => {
                assert_eq!(on_disk.generation, 5);
                assert_eq!(on_disk.state, HostState::Stopped);
            }
            CasOutcome::Applied(_) => panic!("CAS should detect stale generation"),
        }
    }

    fn tempdir_runtime_root() -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runtime").to_string_lossy().into_owned();
        // Leak the TempDir so the path stays alive for the test body.
        std::mem::forget(dir);
        path
    }
