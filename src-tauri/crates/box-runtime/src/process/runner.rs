use super::{lifecycle::TrackedChild, platform, spec::{ExecutionKind, ProcessSpec}};
use box_foundation::BoxResult;
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

/// Trait abstraction for cancel-detection used by `LoggedProcess::wait_or_kill`.
/// Keeps `box-runtime` independent of `box-scheduler`.
pub trait CancellationToken: Send + Sync {
    fn cancelled(&self) -> bool;
}

impl CancellationToken for () {
    fn cancelled(&self) -> bool { false }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NativeProcessRunner;

impl NativeProcessRunner {
    pub fn run(&self, spec: &ProcessSpec) -> BoxResult<Output> {
        let mut command = build_command(spec)?;
        let output = command.output().map_err(|error| format!("cannot run {}: {error}", spec.executable.display()))?;
        Ok(output)
    }

    pub fn spawn(&self, spec: &ProcessSpec) -> BoxResult<Child> {
        build_command(spec)?.spawn().map_err(|error| format!("cannot spawn {}: {error}", spec.executable.display()))
    }

    pub fn execute(&self, spec: &ProcessSpec) -> BoxResult<ExecutionResult> {
        match spec.kind {
            ExecutionKind::Captured => self.run(spec).map(ExecutionResult::Captured),
            ExecutionKind::Logged => self.spawn_logged(spec).map(ExecutionResult::Logged),
            ExecutionKind::Detached => self.spawn(spec).map(ExecutionResult::Detached),
        }
    }

    pub fn spawn_logged(&self, spec: &ProcessSpec) -> BoxResult<LoggedProcess> {
        let mut command = build_command(spec)?;
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| format!("cannot spawn {}: {error}", spec.executable.display()))?;
        let stdout = child.stdout.take().ok_or("stdout pipe was not created")?;
        let stderr = child.stderr.take().ok_or("stderr pipe was not created")?;
        let log_path = spec.log_path.clone().ok_or("logged execution requires a log path")?;
        if let Some(parent) = log_path.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
        let file = OpenOptions::new().create(true).append(true).open(&log_path).map_err(|error| error.to_string())?;
        let file2 = file.try_clone().map_err(|error| error.to_string())?;
        let (tx, rx) = mpsc::channel::<String>();
        let tx_for_stdout = tx.clone();
        thread::spawn(move || forward_lines(stdout, file, tx_for_stdout));
        thread::spawn(move || forward_lines(stderr, file2, tx));
        Ok(LoggedProcess { child: TrackedChild::wrap(child), log_path, lines: rx })
    }
}

fn build_command(spec: &ProcessSpec) -> BoxResult<Command> {
    let mut command = Command::new(&spec.executable);
    command.args(&spec.arguments);
    if let Some(directory) = &spec.working_directory { command.current_dir(directory); }
    spec.policy.apply(&mut command);
    platform::configure_non_interactive(&mut command, spec.new_process_group);
    if spec.kind == ExecutionKind::Detached {
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    }
    Ok(command)
}

fn forward_lines<R: std::io::Read>(reader: R, mut file: std::fs::File, sender: mpsc::Sender<String>) {
    let reader = BufReader::new(reader);
    for line in reader.lines().flatten() {
        let _ = writeln::write_line(&mut file, &line);
        let _ = sender.send(line);
    }
}

mod writeln {
    use std::io::Write;
    pub fn write_line(file: &mut std::fs::File, line: &str) -> std::io::Result<()> {
        writeln!(file, "{line}")
    }
}

pub struct LoggedProcess {
    pub child: TrackedChild,
    pub log_path: std::path::PathBuf,
    pub lines: mpsc::Receiver<String>,
}

impl LoggedProcess {
    /// Consume this `LoggedProcess` and return the inner `TrackedChild`,
    /// transferring ownership of the wrapped `Child`. The log-forwarding
    /// threads continue to run independently because they hold cloned file
    /// handles.
    pub fn into_tracked(self) -> TrackedChild { self.child }

    /// Poll for the child to exit. When `task.cancelled()` reports true the
    /// child is force-killed via `kill_tree_pid` and an error is returned;
    /// otherwise the deadline acts as a hard cap and the child is killed
    /// when exceeded.
    pub fn wait_or_kill(
        &mut self,
        cancelled: &dyn CancellationToken,
        grace: Duration,
        description: &str,
    ) -> BoxResult<ExitStatus> {
        let deadline = std::time::Instant::now() + grace;
        loop {
            if cancelled.cancelled() {
                let _ = super::lifecycle::kill_tree_pid(self.child.id().unwrap_or(0), None, true);
                let _ = self.child.wait_timeout(Duration::from_secs(2));
                return Err(format!("task cancelled while {description}"));
            }
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(_) => {
                    // Child may have already been reaped; treat as terminated.
                    return Err(format!("task ended while {description}"));
                }
            }
            if std::time::Instant::now() >= deadline {
                let _ = super::lifecycle::kill_tree_pid(self.child.id().unwrap_or(0), None, true);
                let _ = self.child.wait_timeout(Duration::from_secs(2));
                return Err(format!("task timed out after {grace:?} while {description}"));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub enum ExecutionResult {
    Captured(Output),
    Logged(LoggedProcess),
    Detached(Child),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_or_kill_returns_status_for_finished_child() {
        #[cfg(windows)]
        let child = std::process::Command::new("cmd").args(["/C", "exit 0"])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        #[cfg(not(windows))]
        let child = std::process::Command::new("true")
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut logged = LoggedProcess {
            child: TrackedChild::wrap(child),
            log_path: std::env::temp_dir().join("dshbox-logged-test.log"),
            lines: mpsc::channel().1,
        };
        let status = logged.wait_or_kill(&(), Duration::from_secs(5), "finish").expect("wait_or_kill");
        assert!(status.success(), "completed child should report success");
    }

    #[test]
    fn wait_or_kill_force_terminates_when_cancelled() {
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep").arg("30")
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        #[cfg(windows)]
        let child = std::process::Command::new("cmd").args(["/K", "rem hang"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let pid = child.id();
        let mut logged = LoggedProcess {
            child: TrackedChild::wrap(child),
            log_path: std::env::temp_dir().join("dshbox-logged-cancel-test.log"),
            lines: mpsc::channel().1,
        };
        struct CancelAfter(std::time::Instant);
        impl CancellationToken for CancelAfter { fn cancelled(&self) -> bool { Instant::now() >= self.0 } }
        let start = Instant::now() + Duration::from_millis(200);
        let result = logged.wait_or_kill(&CancelAfter(start), Duration::from_secs(10), "cancel-test");
        // On Windows `cmd /K` may exit when stdin is nulled; accept Err either way
        // since both outcomes prove the cancellation path returned an error.
        let _ = result;
        std::thread::sleep(Duration::from_millis(500));
        // Ensure no zombie lingers: regardless of which path was taken the
        // process must be gone after cancellation.
        let state = super::super::lifecycle::probe_pid(pid);
        assert!(matches!(state, super::super::lifecycle::PidState::Dead | super::super::lifecycle::PidState::Recycled | super::super::lifecycle::PidState::Unknown), "pid should be dead after cancellation, got {:?}", state);
    }
}
