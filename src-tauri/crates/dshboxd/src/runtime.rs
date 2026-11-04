//! How a container's DSH host is brought up and observed.
//!
//! Today every container runs its host as a plain child process of the
//! daemon. This layer exists so a second backend — a microVM — can be added
//! without touching the verbs above: the daemon's contract with a container
//! is "a loopback URL that answers HTTP with the container's token" plus
//! "a handle that can be observed and stopped", and both hold whether the
//! responder is a process or a virtual machine. The control plane (task
//! queue, host.json, SSE events) and the agent surface (box_* tools over
//! loopback) are already runtime-agnostic; this module is where the
//! execution side is no longer process-shaped (host process or VM handle).

use box_runtime::process::TrackedChild;
use std::time::Duration;

/// Which execution backend a container's host runs on.
///
/// Read from the container record's optional `runtime` field. Absent means
/// [`RuntimeKind::Host`]: containers created before the field existed — and
/// containers whose authors never cared — run exactly as they always did,
/// and the creation path does not write the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    /// A child process of the daemon, on the host OS. The default when a
    /// container declares no VM runtime.
    Host,
    /// A virtual machine. The payload names the requested backend
    /// (`firecracker`, `wsl2`, `whpx`, …); `None` means "any VM backend".
    /// A build that cannot execute the named backend fails the start with
    /// that reason rather than silently degrading to a process: the record
    /// asked for a boundary the process backend cannot provide.
    Vm(Option<String>),
}

impl RuntimeKind {
    /// Read the kind from a parsed `container.json`.
    pub fn from_record(value: &serde_json::Value) -> Result<RuntimeKind, String> {
        match value.get("runtime") {
            None | Some(serde_json::Value::Null) => Ok(RuntimeKind::Host),
            Some(serde_json::Value::String(text)) => RuntimeKind::parse(text),
            Some(other) => Err(format!(
                "container runtime must be a string, got {other}"
            )),
        }
    }

    /// Parse one `runtime` field value. `host` (case-insensitive) is the
    /// explicit process mode; `vm` and `vm:<backend>` request a VM. Empty and
    /// unknown values are errors — the field is a declaration, and silence
    /// here would claim a boundary the process backend cannot give.
    pub fn parse(text: &str) -> Result<RuntimeKind, String> {
        let lower = text.trim().to_ascii_lowercase();
        if lower == "host" {
            return Ok(RuntimeKind::Host);
        }
        if lower == "vm" {
            return Ok(RuntimeKind::Vm(None));
        }
        if let Some(rest) = lower
            .strip_prefix("vm")
            .filter(|rest| matches!(rest.as_bytes().first(), Some(b':') | Some(b'-') | Some(b'_')))
        {
            let backend = rest[1..].trim();
            if backend.is_empty() {
                return Ok(RuntimeKind::Vm(None));
            }
            return Ok(RuntimeKind::Vm(Some(backend.to_owned())));
        }
        Err(format!(
            "unknown container runtime {:?}; supported values are \"host\" and \"vm[:backend]\"",
            text.trim()
        ))
    }

    /// The canonical field spelling, for answers that carry the kind.
    pub fn field(&self) -> String {
        match self {
            RuntimeKind::Host => "host".to_owned(),
            RuntimeKind::Vm(None) => "vm".to_owned(),
            RuntimeKind::Vm(Some(backend)) => format!("vm:{backend}"),
        }
    }
}

/// What the daemon holds once a container's host is up.
///
/// A host process or a VM handle; the backend owns the latter (a VMM API client or
/// a handle to the emulator process) without any verb above knowing.
pub(crate) enum HostHandle {
    /// A child process of this daemon. `TrackedChild` owns the kill/reap
    /// lifecycle; its `Drop` carries the final force-kill exactly as before
    /// this wrapper existed.
    Process(TrackedChild),
    /// A virtual machine, observed and stopped through the backend's handle.
    Vm(Box<dyn crate::vm::VmHandle>),
}

impl HostHandle {
    /// The OS identifier a caller can observe from outside — the host PID
    /// for a process, the VMM PID for a VM.
    pub fn id(&self) -> Option<u32> {
        match self {
            HostHandle::Process(child) => child.id(),
            HostHandle::Vm(vm) => vm.id(),
        }
    }

    /// Process-group id of the host (Linux/macOS); `None` for backends
    /// without one (VM arms, Windows).
    pub fn pgid(&mut self) -> Option<i32> {
        match self {
            HostHandle::Process(child) => child.pgid(),
            HostHandle::Vm(_) => None,
        }
    }

    /// Exit details once the host has exited: `(code, signal)`. A VM arm
    /// reports `None` while it cannot observe the guest's own exit — the
    /// shared health watcher owns liveness for VMs, by URL.
    pub fn exit_status(&mut self) -> std::io::Result<Option<(i32, Option<i32>)>> {
        match self {
            HostHandle::Process(child) => {
                Ok(child.try_wait()?.map(crate::host::exit_status_to_parts))
            }
            HostHandle::Vm(vm) => Ok(if vm.has_exited()? { Some((-1, None)) } else { None }),
        }
    }
    /// Non-blocking liveness: `true` once the host has exited. Deliberately
    /// not `Option<ExitStatus>` — `ExitStatus` has no stable constructor, so
    /// a VM arm could not implement this verb; a VMM answers liveness from
    /// its own API, which is all this method promises.
    pub fn has_exited(&mut self) -> std::io::Result<bool> {
        match self {
            HostHandle::Process(child) => Ok(child.try_wait()?.is_some()),
            HostHandle::Vm(vm) => vm.has_exited(),
        }
    }

    /// Best-effort graceful tree stop. The final force-kill + reap stays
    /// with the handle's own Drop, unchanged.
    pub fn stop(&mut self) {
        match self {
            HostHandle::Process(child) => {
                let _ = child.kill_tree(false, Duration::from_secs(5));
            }
            HostHandle::Vm(vm) => vm.stop(),
        }
    }

    /// Shorter grace for the start-failure / cancel path. Host containers
    /// used a 2s kill_tree here before the VM seam existed; keep that
    /// behaviour instead of inheriting `stop()`'s 5s.
    pub fn stop_abrupt(&mut self) {
        match self {
            HostHandle::Process(child) => {
                let _ = child.kill_tree(false, Duration::from_secs(2));
            }
            HostHandle::Vm(vm) => vm.stop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_null_field_is_host() {
        assert_eq!(RuntimeKind::from_record(&serde_json::json!({})).unwrap(), RuntimeKind::Host);
        assert_eq!(
            RuntimeKind::from_record(&serde_json::json!({ "runtime": null })).unwrap(),
            RuntimeKind::Host
        );
    }

    #[test]
    fn host_is_accepted_in_any_case_and_vm_carries_its_backend() {
        assert_eq!(RuntimeKind::parse("host").unwrap(), RuntimeKind::Host);
        assert_eq!(RuntimeKind::parse("HOST").unwrap(), RuntimeKind::Host);
        assert_eq!(RuntimeKind::parse("vm").unwrap(), RuntimeKind::Vm(None));
        assert_eq!(
            RuntimeKind::parse("vm:firecracker").unwrap(),
            RuntimeKind::Vm(Some("firecracker".to_owned()))
        );
        assert_eq!(
            RuntimeKind::parse("vm-wsl2").unwrap(),
            RuntimeKind::Vm(Some("wsl2".to_owned()))
        );
        // Case handling is uniform: every token is matched on a lowercased
        // copy, so "VM:X" is a VM request, not a mystery string.
        assert_eq!(
            RuntimeKind::parse("VM:X").unwrap(),
            RuntimeKind::Vm(Some("x".to_owned()))
        );
        assert_eq!(
            RuntimeKind::parse("Vm-Wsl2").unwrap(),
            RuntimeKind::Vm(Some("wsl2".to_owned()))
        );
        assert_eq!(RuntimeKind::parse("vm:firecracker").unwrap().field(), "vm:firecracker");
    }

    #[test]
    fn an_unknown_runtime_is_an_error_never_a_silent_host() {
        for attempt in ["docker", "podman", "vmx", "", "   "] {
            let error = RuntimeKind::parse(attempt).unwrap_err();
            assert!(error.contains("unknown container runtime"), "{attempt:?}: {error}");
        }
        let error = RuntimeKind::from_record(&serde_json::json!({ "runtime": 7 })).unwrap_err();
        assert!(error.contains("must be a string"), "{error}");
    }
}