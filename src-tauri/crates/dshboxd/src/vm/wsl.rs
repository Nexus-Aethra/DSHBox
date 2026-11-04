//! Windows backend: one WSL2 distro per container.
//!
//! The container's prepared directory stays on the Windows filesystem and
//! is mounted into the distro through drvfs (`/mnt/<drive>/...`), so the
//! directory-is-the-container model holds without an image pipeline. The
//! distro is imported from the official Ubuntu WSL rootfs tarball (downloaded
//! and cached once; `DSHBOX_WSL_BASE_TAR` overrides it), provisioned with a
//! pinned Node.js build, and the DSH host is launched inside it. WSL2's
//! localhost forwarding exposes the guest's `127.0.0.1:<port>` on the host,
//! which is the loopback contract.
//!
//! A keeper child (`wsl -d <distro> -- sleep infinity`) serves three
//! purposes at once: it gives `host.pid` a real PID to observe, it binds
//! the utility VM's lifetime to the daemon, and its `try_wait` is the
//! backend's liveness answer.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use box_runtime::process::TrackedChild;

use super::{vm_dir, VmHandle};

/// Hide the console window WSL commands would otherwise flash on Windows.
fn silent(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// Run `wsl <args>` and return its decoded stdout. WSL emits UTF-16 for
/// list output, so NULs are stripped unconditionally.
fn wsl_output(args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("wsl");
    command.args(args);
    let output = silent(&mut command)
        .output()
        .map_err(|error| format!("cannot run wsl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "wsl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).replace('\0', ""))
}

/// True when WSL2 is installed and can run distros. A missing or disabled
/// WSL is a start-time error for `vm:wsl2` containers, never a silent
/// degrade to the host backend.
pub fn available() -> Result<(), String> {
    wsl_output(&["--status"])
        .map(|_| ())
        .map_err(|error| format!("WSL2 is not usable for vm:wsl2 containers: {error}"))
}

/// Registered WSL distro names.
fn list_distros() -> Result<Vec<String>, String> {
    Ok(wsl_output(&["--list", "--quiet"])?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The distro name for one container. WSL names allow alphanumerics and
/// dashes, which a `container-<id>` already is.
fn distro_name(container_id: &str) -> String {
    format!("dshbox-{container_id}")
}

/// Pick and cache a base rootfs tarball for `wsl --import`.
///
/// The base image is the official Ubuntu WSL rootfs (340 MB compressed),
/// downloaded once into the system temp directory — deliberately NOT the
/// runtime disk: the tar plus the imported distro's own vhdx would put
/// two gigabyte-scale files on the same volume, and small runtime disks
/// ran out of space in testing. `DSHBOX_WSL_BASE_TAR` overrides the cache
/// with a ready-made uncompressed tar (tests, offline installs).
fn ensure_base_tarball() -> Result<PathBuf, String> {
    let cache = std::env::temp_dir().join("dshbox-wsl-base");
    std::fs::create_dir_all(&cache)
        .map_err(|error| format!("cannot create {}: {error}", cache.display()))?;
    if let Some(custom) = std::env::var_os("DSHBOX_WSL_BASE_TAR") {
        let path = PathBuf::from(custom);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "DSHBOX_WSL_BASE_TAR points at a missing file: {}",
            path.display()
        ));
    }
    let base = cache.join("wsl-base.tar");
    if base.is_file() {
        return Ok(base);
    }
    let gz = cache.join("ubuntu-noble-wsl-amd64-wsl.rootfs.tar.gz");
    if !gz.is_file() {
        // Stream into a temporary file and rename only on success: an
        // interrupted download must not be cached as if it were complete.
        let partial = cache.join("ubuntu-noble-wsl-amd64-wsl.rootfs.tar.gz.part");
        let _ = fs::remove_file(&partial);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1800))
            .build()
            .map_err(|error| format!("cannot build download client: {error}"))?;
        let mut response = client
            .get("https://cloud-images.ubuntu.com/wsl/releases/24.04/current/ubuntu-noble-wsl-amd64-wsl.rootfs.tar.gz")
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|error| format!("cannot download the WSL base rootfs: {error}"))?;
        let mut file = fs::File::create(&partial)
            .map_err(|error| format!("cannot create {}: {error}", partial.display()))?;
        std::io::copy(&mut response, &mut file)
            .map_err(|error| format!("cannot download the WSL base rootfs: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("cannot flush {}: {error}", partial.display()))?;
        drop(file);
        fs::rename(&partial, &gz).map_err(|error| format!("cannot publish the WSL base rootfs: {error}"))?;
    }
    // Verify the pinned digest before anything is unpacked or imported,
    // including a gz that was cached by an earlier run.
    let gz_bytes = fs::read(&gz)
        .map_err(|error| format!("cannot read {}: {error}", gz.display()))?;
    super::verify_sha256(&gz_bytes, super::WSL_BASE_SHA256)
        .map_err(|error| format!("WSL base rootfs: {error}"))?;
    // Un-gzip into an uncompressed tar: `wsl --import` wants a plain tar.
    // Write to .part too, so a crash mid-unpack cannot leave a truncated
    // tar that every later start would trust.
    let partial_tar = cache.join("wsl-base.tar.part");
    let _ = fs::remove_file(&partial_tar);
    let input = fs::File::open(&gz).map_err(|error| error.to_string())?;
    let mut output = fs::File::create(&partial_tar).map_err(|error| error.to_string())?;
    std::io::copy(
        &mut flate2::read::GzDecoder::new(input),
        &mut output,
    )
    .map_err(|error| format!("cannot unpack the WSL base rootfs: {error}"))?;
    output.sync_all().map_err(|error| error.to_string())?;
    drop(output);
    fs::rename(&partial_tar, &base)
        .map_err(|error| format!("cannot publish the WSL base tar: {error}"))?;
    Ok(base)
}

/// One started WSL2 container.
pub struct WslVm {
    distro: String,
    keeper: TrackedChild,
}

impl WslVm {
    /// Import (first start only), provision and launch the DSH host inside
    /// a dedicated distro, then wait for the port to answer on the host.
    pub fn start(
        container_id: &str,
        container_directory: &Path,
        profile: &str,
        // The port itself travels through the web patch file, written by
        // lifecycle before the backend is invoked.
        _port: u16,
        task: Option<&box_scheduler::TaskContext>,
    ) -> Result<WslVm, String> {
        available()?;
        let distro = distro_name(container_id);
        let registered = list_distros()?;
        let vm_dir = vm_dir(container_directory);
        std::fs::create_dir_all(&vm_dir)
            .map_err(|error| format!("cannot create {}: {error}", vm_dir.display()))?;

        if !registered.iter().any(|name| name == &distro) {
            let base = ensure_base_tarball()
                .map_err(|error| format!("vm:wsl2 base image unavailable: {error}"))?;
            if let Some(task) = task {
                task.log(&format!("importing WSL distro {distro} from {}", base.display()));
            }
            wsl_output(&[
                "--import",
                &distro,
                &vm_dir.join("distro").to_string_lossy(),
                &base.to_string_lossy(),
            ])?;
            // The fresh rootfs has no Node yet. Drop the Windows-side marker
            // so a re-import (manual unregister, WSL reset) actually
            // re-provisions instead of launching a guest without node.
            let _ = fs::remove_file(vm_dir.join("provisioned"));
        }

        // Provision once: a pinned Node.js build extracted into /usr/local.
        // The marker lives on the Windows side, and is invalidated on import.
        let marker = vm_dir.join("provisioned");
        if !marker.is_file() {
            if let Some(task) = task {
                task.log("provisioning WSL distro with Node.js");
            }
            let node_tar = vm_dir.join("node.tar.xz");
            if !node_tar.is_file() {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(300))
                    .build()
                    .map_err(|error| format!("cannot build download client: {error}"))?;
                let bytes = client
                    .get(super::guest_node_url())
                    .send()
                    .and_then(|response| response.error_for_status())
                    .and_then(|response| response.bytes())
                    .map_err(|error| format!("cannot download guest Node.js: {error}"))?;
                std::fs::write(&node_tar, &bytes)
                    .map_err(|error| format!("cannot write {}: {error}", node_tar.display()))?;
            }
            let guest_tar = super::windows_to_wsl_path(&node_tar)
                .ok_or("node tarball is not on a fixed drive")?;
            wsl_output(&[
                "-d",
                &distro,
                "-u",
                "root",
                "--exec",
                "tar",
                "-xJf",
                &guest_tar,
                "-C",
                "/usr/local",
                "--strip-components=1",
            ])?;
            fs::write(&marker, super::guest_node_url()).map_err(|error| error.to_string())?;
        }

        // A keeper keeps the utility VM alive and owns the PID host.json
        // records. It dies with the daemon (kill on shutdown) and its
        // try_wait is the backend's liveness answer.
        let keeper = silent(
            Command::new("wsl")
                .args(["-d", &distro, "--exec", "sleep", "infinity"]),
        )
        .spawn()
        .map_err(|error| format!("cannot start WSL keeper: {error}"))?;
        let keeper = TrackedChild::wrap(keeper);

        let vm = WslVm { distro, keeper };
        vm.launch_dsh(container_id, container_directory, profile)?;
        Ok(vm)
    }

    /// Launch the DSH host inside the distro. The guest sees the prepared
    /// directory through drvfs, binds `127.0.0.1:<port>` inside the VM, and
    /// WSL2's localhost forwarding exposes it on the host loopback.
    fn launch_dsh(
        &self,
        container_id: &str,
        container_directory: &Path,
        profile: &str,
    ) -> Result<(), String> {
        // `profile` is interpolated into a `bash -c` script below. It comes
        // from container.json, which is local user data, but a stray shell
        // metacharacter would still turn into command execution inside the
        // guest. Validate before any path is built.
        if !box_foundation::is_safe_identifier(profile) {
            return Err(format!("invalid profile for vm:wsl2: {profile:?}"));
        }
        let guest = |path: &Path| {
            super::windows_to_wsl_path(path).ok_or_else(|| {
                format!("{} is not on a fixed drive; vm:wsl2 containers must live on one", path.display())
            })
        };
        let harness = guest(&container_directory.join("harness"))?;
        let home = guest(&container_directory.join("profile"))?;
        let patch = guest(&container_directory.join("box-web.patch.yml"))?;
        let log = guest(&container_directory.join("logs").join("host.log"))?;
        // Pass every dynamic value as a positional parameter, never
        // interpolated into the script text: runtime directories and
        // container.json fields may contain spaces or shell metacharacters.
        // `bash -c <script> <name> <arg>...` puts <name> in $0 and the
        // arguments in $1..$6.
        let script = r#"cd "$1" || exit 1
DSH_HOME="$2" DSHBOX_CONTAINER_ID="$3" setsid nohup /usr/local/bin/node apps/cli/lib/bin.js --profile "$4" --patch "$5" --no-open < /dev/null >> "$6" 2>&1 &
sleep 2"#;
        let launched = silent(
            Command::new("wsl").args([
                "-d", &self.distro, "--exec", "bash", "-c", script,
                "dshbox-launch", &harness, &home, container_id, profile, &patch, &log,
            ]),
        )
        .output()
        .map_err(|error| format!("cannot launch DSH host in WSL: {error}"))?;
        if !launched.status.success() {
            return Err(format!(
                "cannot launch DSH host in WSL (exit {}): {}",
                launched.status,
                String::from_utf8_lossy(&launched.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Graceful stop: TERM the DSH host inside the distro, then tear the
    /// utility VM down. The keeper is killed last by the handle wrapper.
    pub fn stop_wsl(&mut self) {
        let _ = silent(
            Command::new("wsl")
                .args([
                    "-d",
                    &self.distro,
                    "-u",
                    "root",
                    "--exec",
                    "pkill",
                    "-TERM",
                    "-f",
                    "apps/cli/lib/bin.js",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
        )
        .status();
        let _ = wsl_output(&["--terminate", &self.distro]);
    }

}

impl VmHandle for WslVm {
    fn id(&self) -> Option<u32> {
        self.keeper.id()
    }

    fn has_exited(&mut self) -> std::io::Result<bool> {
        // The keeper dying means the VM is gone. The DSH host inside can
        // still die first; the shared health watcher catches that by URL.
        self.keeper.try_wait().map(|status| status.is_some())
    }

    fn stop(&mut self) {
        self.stop_wsl();
        let _ = self.keeper.kill_tree(false, Duration::from_secs(2));
    }
}
/// Remove the distro of a deleted container, if it is still registered.
/// Best-effort by contract: an already-unregistered distro is not an error.
pub fn unregister_container(container_id: &str) -> Result<(), String> {
    let distro = distro_name(container_id);
    if list_distros()?.iter().any(|name| name == &distro) {
        wsl_output(&["--unregister", &distro])?;
    }
    Ok(())
}
