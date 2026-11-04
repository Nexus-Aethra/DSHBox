//! Virtual-machine backends for `RuntimeKind::Vm`.
//!
//! One module per platform backend, one [`VmHandle`] trait the daemon's
//! verbs talk to. A backend's whole contract with the rest of the daemon:
//! bring the container's prepared directory up inside an isolated
//! environment, expose the DSH port on a host loopback port, and offer the
//! three handle operations (`id`, `has_exited`, `stop`). Everything above —
//! readiness probing, host.json, health watching, the agent surface — is
//! backend-agnostic.

pub mod libkrun;
pub mod wsl;

use std::path::{Path, PathBuf};

/// A started VM, as the daemon's verbs may observe and stop it.
pub trait VmHandle: Send {
    /// An OS identifier for outside observers: the keeper/VMM process id.
    fn id(&self) -> Option<u32>;
    /// Non-blocking liveness: `true` once the VM has gone away.
    fn has_exited(&mut self) -> std::io::Result<bool>;
    /// Best-effort graceful stop of the guest workload and the VM itself.
    fn stop(&mut self);
}

/// Map a Windows-side container path to the guest path WSL2 mounts it at
/// (`D:\dshbox\...` → `/mnt/d/dshbox/...`). Only meaningful for paths on
/// fixed drives, which is where every runtime path lives.
pub(crate) fn windows_to_wsl_path(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = &text[2..].replace('\\', "/");
    Some(format!("/mnt/{drive}{rest}"))
}

/// Node.js LTS the VM guests provision (Linux x64 build from nodejs.org).
/// Pinned like every other runtime artifact; bumped deliberately.
pub const GUEST_NODE_VERSION: &str = "22.20.0";

/// SHA-256 of node-v22.20.0-linux-x64.tar.xz, from
/// https://nodejs.org/dist/v22.20.0/SHASUMS256.txt
#[cfg(target_os = "linux")]
pub const GUEST_NODE_SHA256: &str =
    "00bbd05e306ea68b6e13e17360d0e2f680b493ef95f2fea1c4296ff7437530bc";

/// SHA-256 of Ubuntu 24.04 WSL amd64 rootfs, from
/// https://cloud-images.ubuntu.com/wsl/releases/24.04/current/SHA256SUMS
/// (the `ubuntu-noble-wsl-amd64-wsl.rootfs.tar.gz` row).
pub const WSL_BASE_SHA256: &str =
    "8251e27ffff381a4af5f41dcb94d867de3e0d9774a9241908ab34555d99315ea";

/// Verify a downloaded artifact against a pinned hex digest. The digest is
/// the trust anchor; a mismatch aborts, never falls back or retries silently.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(format!(
            "artifact digest mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

pub fn guest_node_url() -> String {
    format!(
        "https://nodejs.org/dist/v{GUEST_NODE_VERSION}/node-v{GUEST_NODE_VERSION}-linux-x64.tar.xz"
    )
}

/// Scratch parent for VM working files, derived from the container dir.
pub(crate) fn vm_dir(container_directory: &Path) -> PathBuf {
    container_directory.join("vm")
}

/// Verify a requested VM backend is usable on this platform and build,
/// before any start side effect. The platform default answers for `None`.
pub fn check_backend(backend: Option<&str>) -> Result<(), String> {
    match backend {
        #[cfg(target_os = "windows")]
        Some("wsl2") => wsl::available(),
        #[cfg(target_os = "linux")]
        Some("libkrun") => libkrun::available(),
        // No backend named: the platform default.
        None => {
            let backend = default_backend()?;
            check_backend(Some(&backend))
        }
        Some(other) => Err(format!(
            "unknown or unsupported VM backend {other:?} on this platform; supported: {}",
            supported_list()
        )),
    }
}

/// The platform's default VM backend, for records that ask for `vm`
/// without naming one.
pub fn default_backend() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        Ok("wsl2".to_owned())
    }
    #[cfg(target_os = "linux")]
    {
        Ok("libkrun".to_owned())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Err("this platform has no VM backend".to_owned())
    }
}

fn supported_list() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "wsl2"
    }
    #[cfg(target_os = "linux")]
    {
        "libkrun"
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        "(none)"
    }
}

/// Bring one container's host up inside the requested VM backend. Returns
/// once the backend has done its own launch steps; the shared readiness
/// probe in lifecycle decides when the port actually answers.
pub fn launch(
    container_id: &str,
    container_directory: &Path,
    profile: &str,
    port: u16,
    backend: Option<&str>,
    task: Option<&box_scheduler::TaskContext>,
) -> Result<Box<dyn VmHandle>, String> {
    match backend {
        #[cfg(target_os = "windows")]
        Some("wsl2") | None => Ok(Box::new(wsl::WslVm::start(
            container_id,
            container_directory,
            profile,
            port,
            task,
        )?)),
        #[cfg(target_os = "linux")]
        Some("libkrun") | None => Ok(Box::new(
            libkrun::LibkrunVm::start(container_id, container_directory, profile, port, task)?
        )),
        Some(other) => Err(format!(
            "VM backend {other:?} is not available on this platform; supported: {}",
            supported_list()
        )),
        // No platform arm above matched (macOS and any future host): fail
        // with the same honest error rather than leaving the match
        // non-exhaustive. Without this arm the crate does not compile on a
        // host with no VM backend, which broke the macOS release build.
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        None => Err("this platform has no VM backend".to_owned()),
    }
}