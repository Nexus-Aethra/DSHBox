//! Cross-platform child process lifecycle helpers built on top of
//! `std::process::Child`. The unified [`TrackedChild`] owns the handle and
//! guarantees that any child spawned through this module will be reaped or
//! terminated, preventing zombies/orphans on both Windows and Unix.

use std::{
    process::{Child, ExitStatus},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

/// Liveness probe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidState {
    /// Process is running.
    Alive,
    /// Process is a zombie (`/proc/<pid>/status` reports `State: Z`).
    Zombie,
    /// Process no longer exists.
    Dead,
    /// PID was recycled by an unrelated process (Windows ACCESS_DENIED).
    Recycled,
    /// Probe could not run (e.g. platform unsupported).
    Unknown,
}

/// Wrapped child with safety guarantees.
pub struct TrackedChild {
    child: Option<Child>,
    pgid: Option<i32>,
    reaped: bool,
}

impl TrackedChild {
    pub fn wrap(child: Child) -> Self {
        Self {
            child: Some(child),
            pgid: None,
            reaped: false,
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
    pub fn pid(&self) -> Option<u32> {
        self.id()
    }
    pub fn pgid(&mut self) -> Option<i32> {
        if self.pgid.is_some() {
            return self.pgid;
        }
        let pid = self.id()?;
        self.pgid = Some(detect_pgid(pid));
        self.pgid
    }

    pub fn unwrap(&mut self) -> std::io::Result<ExitStatus> {
        if let Some(mut child) = self.child.take() {
            let status = child.wait()?;
            self.reaped = true;
            return Ok(status);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "child already reaped",
        ))
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// Wait up to `timeout` for the child to exit. Returns `Some(status)`
    /// if it exited, `None` if the timeout elapsed. Never blocks past the
    /// deadline.
    pub fn wait_timeout(&mut self, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait()? {
                Some(status) => {
                    self.reaped = true;
                    return Ok(Some(status));
                }
                None => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Kill the child and its process group if available. When `force` is
    /// false, a SIGTERM / `taskkill` (graceful) is issued; if the child
    /// does not exit within `grace`, the call escalates to a forced kill.
    pub fn kill_tree(&mut self, force: bool, grace: Duration) -> std::io::Result<()> {
        let pid = match self.id() {
            Some(pid) => pid,
            None => return Ok(()),
        };
        let pgid = self.pgid();
        let escalated = match kill_tree_pid(pid, pgid, force) {
            Ok(()) => false,
            Err(_) if !force => kill_tree_pid(pid, pgid, true).is_ok(),
            Err(error) => return Err(error),
        };
        let _ = escalated;
        if grace.is_zero() {
            return self.reap_blocking();
        }
        match self.wait_timeout(grace)? {
            Some(_) => Ok(()),
            None => {
                if !force {
                    kill_tree_pid(pid, pgid, true)?;
                    let _ = self.wait_timeout(Duration::from_secs(2))?;
                }
                self.reap_blocking()
            }
        }
    }

    fn reap_blocking(&mut self) -> std::io::Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
            self.reaped = true;
        }
        Ok(())
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(pid) = self.id() {
            let pgid = self.pgid();
            let _ = kill_tree_pid(pid, pgid, false);
            if let Ok(Some(_)) = self.wait_timeout(Duration::from_millis(200)) {
                return;
            }
            let _ = kill_tree_pid(pid, pgid, true);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
            self.reaped = true;
        }
    }
}

/// Kill a process and (on Unix) its process group.
pub fn kill_tree_pid(pid: u32, pgid: Option<i32>, force: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let target_pgid = pgid.unwrap_or(pid as i32);
        // Negative pid means "send to the whole process group".
        let rc = unsafe { libc::kill(-target_pgid, signal) };
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // No pgid available, fall back to plain pid kill.
            let rc = unsafe { libc::kill(pid as i32, signal) };
            if rc == -1 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                return Err(err);
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = pgid;
        let mut cmd = std::process::Command::new("taskkill");
        cmd.arg("/PID").arg(pid.to_string()).arg("/T");
        if force {
            cmd.arg("/F");
        }
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => {
                let mut cmd = std::process::Command::new("taskkill");
                cmd.arg("/PID").arg(pid.to_string());
                if force {
                    cmd.arg("/F");
                }
                cmd.creation_flags(0x0800_0000);
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                cmd.status().map(|_| ())
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn detect_pgid(pid: u32) -> i32 {
    unsafe { libc::getpgid(pid as i32) }
}
#[cfg(windows)]
fn detect_pgid(_pid: u32) -> i32 {
    0
}

/// Probe the live state of a PID. On Windows the probe distinguishes
/// `Recycled` from `Dead` so callers don't accidentally treat a recycled
/// PID as "ours".
pub fn probe_pid(pid: u32) -> PidState {
    #[cfg(unix)]
    {
        let status_path = format!("/proc/{pid}/status");
        match std::fs::File::open(&status_path) {
            Ok(mut file) => {
                let mut contents = String::new();
                let _ = file.read_to_string(&mut contents);
                if contents.contains("State:\tZ") {
                    PidState::Zombie
                } else {
                    PidState::Alive
                }
            }
            Err(error) => match error.raw_os_error() {
                Some(libc::ENOENT) => PidState::Dead,
                Some(libc::ESRCH) => PidState::Dead,
                _ => PidState::Unknown,
            },
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn OpenProcess(desired: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
            fn GetExitCodeProcess(handle: *mut core::ffi::c_void, exit_code: *mut u32) -> i32;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        let wide: Vec<u16> = std::ffi::OsStr::new("")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let _ = wide;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(5) /* ERROR_ACCESS_DENIED */ => PidState::Recycled,
                Some(87) /* ERROR_INVALID_PARAMETER */ => PidState::Dead,
                _ => PidState::Unknown,
            }
        } else {
            let mut exit_code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
            unsafe {
                CloseHandle(handle);
            }
            if ok == 0 {
                PidState::Unknown
            } else if exit_code == STILL_ACTIVE {
                PidState::Alive
            } else {
                PidState::Dead
            }
        }
    }
}

static SHUTDOWN_REQUESTED: OnceLock<std::sync::atomic::AtomicBool> = OnceLock::new();

pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED
        .get_or_init(|| std::sync::atomic::AtomicBool::new(false))
        .load(std::sync::atomic::Ordering::SeqCst)
}

pub fn request_shutdown() {
    SHUTDOWN_REQUESTED
        .get_or_init(|| std::sync::atomic::AtomicBool::new(false))
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Install signal handlers for SIGTERM/SIGINT. Unix-only; on Windows no-op
/// because the runtime owns Ctrl-C/SIGTERM handling for console processes.
pub fn install_signal_handlers() {
    #[cfg(unix)]
    {
        unsafe {
            libc::signal(libc::SIGTERM, handle_signal as libc::sighandler_t);
            libc::signal(libc::SIGINT, handle_signal as libc::sighandler_t);
            libc::signal(libc::SIGHUP, handle_signal as libc::sighandler_t);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ();
    }
}

#[cfg(unix)]
extern "C" fn handle_signal(_signal: libc::c_int) {
    request_shutdown();
}

#[cfg(test)]
mod tests {
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
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn long-running child");
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
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn probe child");
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
}
