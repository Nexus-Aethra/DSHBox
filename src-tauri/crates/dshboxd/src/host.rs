//! Persistent host process record (`host.json`) for running containers.
//!
//! The daemon writes this file each time a container's host process
//! transitions state (start / ready / unhealthy / crash / stop). On
//! daemon restart, the file is read to reconcile orphaned containers
//! against the live process table. Each write is gated by a compare-
//! and-swap on `generation`: a writer who loaded generation N only
//! accepts a record whose on-disk generation is ≤ N, and bumps it to
//! N+1 before persisting. This prevents a stale watcher (left behind
//! by a previous daemon instance) from clobbering a fresh entry.
//!
//! Cross-platform: `hostPid` + `hostPgid` (Linux/macOS process group
//! id; 0 on Windows where process groups don't exist) plus `exitSignal`
//! (Some on unix, None on Windows).

use box_containers::container_directory;
use box_foundation::{now_seconds, read_config, BoxResult};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf, process::ExitStatus};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Host lifecycle state. Persisted as lowercase snake_case so JSON
/// stays readable to external tooling (`jq`, the desktop UI, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// Host process spawned but URL has not yet answered.
    Starting,
    /// First successful HTTP probe since spawn.
    Ready,
    /// Ready + at least one recent successful probe.
    Running,
    /// `try_wait` saw an exit OR `unhealthy_count` crossed the threshold.
    Crashed,
    /// Explicit `container stop` from a user.
    Stopped,
    /// Daemon restart found the recorded PID alive but EPERM'd
    /// (likely re-used by an unrelated process). Requires manual restart.
    Orphaned,
    /// Container was created but never reached a healthy host process —
    /// e.g. a plugin build/preflight step failed before the host was
    /// spawned. The directory and metadata exist but the container is
    /// not startable until the user fixes the underlying issue (rebuild,
    /// delete-and-recreate, etc.). Distinct from `Stopped` (user action)
    /// and `Crashed` (host process died after starting).
    Corrupted,
}

/// Persisted shape of `state/host.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerHostRecord {
    pub id: String,
    pub name: String,
    pub template: Option<String>,
    pub profile: String,
    pub host_pid: u32,
    /// Process group id (Linux/macOS). 0 on Windows where the concept
    /// does not exist; cleanup uses Job Objects there instead.
    pub host_pgid: i32,
    pub host_port: u16,
    pub host_url: String,
    pub started_at: u64,
    pub last_seen: u64,
    pub state: HostState,
    pub generation: u64,
    pub exit_status: Option<i32>,
    pub exit_signal: Option<i32>,
    pub unhealthy_count: u32,
    pub probe_count: u64,
}

/// Path to the record file inside a container's `state/` directory.
pub fn host_record_path(runtime_root: &str, id: &str) -> PathBuf {
    container_directory(runtime_root, id)
        .join("state")
        .join("host.json")
}

/// Read the record for `id`, or `None` if the file is absent / corrupt.
pub fn read_host_record(id: &str) -> BoxResult<Option<ContainerHostRecord>> {
    let Some(root) = read_config()?.runtime_directory else {
        return Ok(None);
    };
    read_host_record_in(&root, id)
}

/// Read `host.json` under a specific runtime root. Public so tests can
/// drive the CAS logic without rewriting `~/.dsh-box/config.json`.
pub fn read_host_record_in(runtime_root: &str, id: &str) -> BoxResult<Option<ContainerHostRecord>> {
    let path = host_record_path(runtime_root, id);
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<ContainerHostRecord>(&text) {
            Ok(record) => Ok(Some(record)),
            Err(error) => {
                eprintln!("host.json for {id} is corrupt ({error}); treating as missing");
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read {path}: {error}",
            path = path.display()
        )),
    }
}

/// Atomic write under a specific runtime root.
pub fn write_host_record_in(runtime_root: &str, record: &ContainerHostRecord) -> BoxResult<()> {
    let path = host_record_path(runtime_root, &record.id);
    let parent = path
        .parent()
        .ok_or_else(|| format!("host.json path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temporary = parent.join("host.json.tmp");
    let serialised = serde_json::to_string_pretty(record)
        .map_err(|error| format!("cannot serialise host record: {error}"))?;
    fs::write(&temporary, serialised)
        .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("cannot rename to {}: {error}", path.display()))?;
    Ok(())
}

/// Atomic write: serialise `record` to a `host.json.tmp` sibling then
/// rename over the existing file. POSIX `rename` is atomic on the same
/// filesystem, so a concurrent reader never sees a half-written file.
pub fn write_host_record(record: &ContainerHostRecord) -> BoxResult<()> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    write_host_record_in(&root, record)
}

/// Outcome of [`compare_and_swap_host_record`].
#[derive(Debug)]
#[allow(dead_code)] // fields are read by integration tests, not by daemon internals.
pub enum CasOutcome {
    /// Caller's update was persisted; `record` now reflects the new state.
    Applied(ContainerHostRecord),
    /// The on-disk generation was higher than the caller's snapshot,
    /// meaning a fresher writer (the active daemon) already touched the
    /// record. Caller should drop its update and re-read.
    Stale { on_disk: ContainerHostRecord },
}

/// Compare-and-swap under a specific runtime root (test seam).
pub fn compare_and_swap_host_record_in<F>(
    runtime_root: &str,
    id: &str,
    snapshot: &ContainerHostRecord,
    mutate: F,
) -> BoxResult<CasOutcome>
where
    F: FnOnce(&ContainerHostRecord) -> ContainerHostRecord,
{
    let path = host_record_path(runtime_root, id);
    let on_disk = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<ContainerHostRecord>(&text)
            .map_err(|error| format!("cannot parse host.json for {id}: {error}"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => snapshot.clone(),
        Err(error) => {
            return Err(format!(
                "cannot read {path}: {error}",
                path = path.display()
            ))
        }
    };
    if on_disk.generation > snapshot.generation {
        return Ok(CasOutcome::Stale { on_disk });
    }
    let mut next = mutate(&on_disk);
    next.generation = on_disk.generation + 1;
    write_host_record_in(runtime_root, &next)?;
    Ok(CasOutcome::Applied(next))
}

/// Read `host.json`, run `mutate` to derive the next record, and only
/// persist it when the on-disk `generation` matches the snapshot the
/// caller already loaded. Generation is bumped before writing.
///
/// `mutate` must NOT mutate its input — return a fresh record derived
/// from the snapshot. The function will atomically overwrite the file
/// if generation matches; otherwise it returns `Stale` and the caller
/// can decide whether to retry.
pub fn compare_and_swap_host_record<F>(
    id: &str,
    snapshot: &ContainerHostRecord,
    mutate: F,
) -> BoxResult<CasOutcome>
where
    F: FnOnce(&ContainerHostRecord) -> ContainerHostRecord,
{
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    compare_and_swap_host_record_in(&root, id, snapshot, mutate)
}

/// Unlink the record file. Called when the daemon confirms the host is
/// permanently dead and the user must run `dshbox container start`
/// again to recover (no auto-restart policy).
pub fn remove_host_record(id: &str) {
    if let Ok(path) = host_json_path_for(id) {
        let _ = fs::remove_file(path);
    }
}

/// Build the canonical record for a freshly-spawned host. Used by the
/// start path right after `Command::spawn` succeeds and before the
/// readiness probe loop begins.
pub fn initial_record(
    id: &str,
    name: &str,
    template: Option<&str>,
    profile: &str,
    host_pid: u32,
    host_pgid: i32,
    host_port: u16,
    host_url: &str,
) -> ContainerHostRecord {
    let started_at = now_seconds();
    ContainerHostRecord {
        id: id.to_owned(),
        name: name.to_owned(),
        template: template.map(str::to_owned),
        profile: profile.to_owned(),
        host_pid,
        host_pgid,
        host_port,
        host_url: host_url.to_owned(),
        started_at,
        last_seen: started_at,
        state: HostState::Starting,
        generation: 1,
        exit_status: None,
        exit_signal: None,
        unhealthy_count: 0,
        probe_count: 0,
    }
}

/// Split an `ExitStatus` into a code + optional signal so we can
/// persist both fields. On unix the signal is captured for crashes
/// (SIGSEGV, SIGABRT, ...); on Windows it's always `None`.
pub fn exit_status_to_parts(status: ExitStatus) -> (i32, Option<i32>) {
    #[cfg(unix)]
    {
        let code = status.code().unwrap_or(-1);
        let signal = status.signal();
        (code, signal)
    }
    #[cfg(not(unix))]
    {
        (status.code().unwrap_or(-1), None)
    }
}

fn host_json_path_for(id: &str) -> BoxResult<PathBuf> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(host_record_path(&root, id))
}

/// Walk every `state/host.json` under the runtime root and return them
/// in id-sorted order. Used by `reconcile_orphan_containers` at
/// daemon startup. Missing or corrupt files are silently skipped (the
/// caller treats them as already-cleaned).
pub fn list_all_host_records() -> BoxResult<Vec<ContainerHostRecord>> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    list_all_host_records_in(&root)
}

/// Same as [`list_all_host_records`] but takes an explicit runtime
/// root. Used by tests; production code should call the no-suffix
/// variant.
pub fn list_all_host_records_in(runtime_root: &str) -> BoxResult<Vec<ContainerHostRecord>> {
    let state_root = PathBuf::from(runtime_root).join("instances");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&state_root) {
        Ok(e) => e,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(format!("cannot list instances: {error}")),
    };
    for entry in entries.flatten() {
        let path = entry.path().join("state").join("host.json");
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(record) = serde_json::from_str::<ContainerHostRecord>(&text) {
                out.push(record);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Helper: produce a process description from an exit pair for logging.
#[allow(dead_code)] // exercised by integration tests; daemon core uses describe_exit_inline.
pub fn describe_exit(status: Option<i32>, signal: Option<i32>) -> String {
    match (status, signal) {
        (_, Some(sig)) => format!("signal {sig}"),
        (Some(code), _) => format!("exit code {code}"),
        (None, None) => "unknown exit".to_owned(),
    }
}

/// Write a `host.json` whose state is `Corrupted` — used as a sentinel
/// before preflight so that a failure there leaves a distinguishable
/// record instead of no record at all (which the UI would show as
/// `Stopped`, conflating "never started" with "user stopped it").
pub fn write_corrupted_record(
    id: &str,
    name: &str,
    template: Option<&str>,
    profile: &str,
    reason: &str,
) -> BoxResult<()> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = host_record_path(&root, id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create host state directory: {error}"))?;
    }
    let record = ContainerHostRecord {
        id: id.to_owned(),
        name: name.to_owned(),
        template: template.map(str::to_owned),
        profile: profile.to_owned(),
        host_pid: 0,
        host_pgid: 0,
        host_port: 0,
        host_url: String::new(),
        started_at: now_seconds(),
        last_seen: 0,
        state: HostState::Corrupted,
        generation: 1,
        exit_status: None,
        exit_signal: None,
        unhealthy_count: 0,
        probe_count: 0,
    };
    // Store the failure reason in the record via a diagnostic field if
    // one exists; otherwise we rely on the task log. For now the record
    // alone is enough for the UI to distinguish the state.
    let _ = reason; // suppress unused warning; diagnostic is in task log
    write_host_record_in(&root, &record)
}

#[cfg(test)]
mod tests {
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
}
