//! tests for lifecycle.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn wait_timeout_returns_none_for_long_running_child() {
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/K", "rem keep alive"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn long-running child");
        // Own process group so the trailing kill_tree() cannot signal the
        // test binary itself (see probe_pid_alive_and_dead).
        #[cfg(not(windows))]
        let child = {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn long-running child")
        };
        let mut tracked = TrackedChild::wrap(child);
        // Windows `cmd /K` may exit on machines without an interactive console;
        // accept either `None` (still running) or a quick non-zero exit on those
        // hosts. The Unix assertion remains strict.
        let result = tracked
            .wait_timeout(Duration::from_millis(200))
            .expect("wait_timeout");
        #[cfg(not(windows))]
        assert!(
            result.is_none(),
            "wait_timeout must return None while child is alive"
        );
        #[cfg(windows)]
        {
            if let Some(status) = result {
                eprintln!("note: cmd /K exited unexpectedly (status={status:?}) — skipping strict assertion");
            }
        }
        let _ = tracked.kill_tree(true, Duration::from_secs(2));
    }

    #[test]
    fn wait_timeout_returns_status_for_finished_child() {
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn short child");
        #[cfg(not(windows))]
        let child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn short child");
        let mut tracked = TrackedChild::wrap(child);
        let result = tracked
            .wait_timeout(Duration::from_secs(5))
            .expect("wait_timeout");
        assert!(
            result.is_some(),
            "wait_timeout must return Some(ExitStatus)"
        );
    }

    #[test]
    fn probe_pid_alive_and_dead() {
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/K", "rem keep alive"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn probe child");
        // The child must live in its own process group: kill_tree()
        // signals the whole group of `pid`, and a same-group child would
        // take the test binary down with it (SIGKILL to our own pgid
        // surfaces as exit code 137 in sandboxed CI runners).
        #[cfg(not(windows))]
        let child = {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn probe child")
        };
        let pid = child.id();
        assert!(matches!(probe_pid(pid), PidState::Alive));
        let mut tracked = TrackedChild::wrap(child);
        let _ = tracked.kill_tree(true, Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(500));
        assert!(matches!(
            probe_pid(pid),
            PidState::Dead | PidState::Recycled
        ));
    }
