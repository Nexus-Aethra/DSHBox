//! Linux backend: one libkrun microVM per container, driven in-process.
//!
//! libkrun (1.9.x) bundles its own Linux kernel, so there is no image to
//! manage — only a guest root directory (`krun_set_root`), the container's
//! prepared directory shared through virtiofs (`krun_add_virtiofs`), and
//! the exec to run (`krun_set_exec`). Networking defaults to TSI: the
//! guest shares the host's network stack, so a DSH host binding
//! `127.0.0.1:<port>` inside the VM binds the host's loopback directly —
//! no port forwarding layer exists at all.
//!
//! The VMM runs in a dedicated child process (`dshboxd --vmm-run`) rather
//! than a daemon thread: `krun_start_enter` blocks for the lifetime of
//! the VM, and a blocking call inside the daemon could neither be observed
//! nor stopped. The child is a `TrackedChild` like any host process, so
//! pid/observe/stop semantics are exactly the process backend's.
//!
//! libkrun is loaded with dlopen at runtime (no link-time dependency), so
//! this backend compiles everywhere and fails at start with a precise
//! error when the host lacks the library or `/dev/kvm`.

#[cfg(target_os = "linux")]
pub mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use box_runtime::process::TrackedChild;

    use super::super::{guest_node_url, vm_dir, VmHandle};

    /// True when the host can actually run KVM microVMs.
    pub fn available() -> Result<(), String> {
        if !Path::new("/dev/kvm").exists() {
            return Err("/dev/kvm is not available; vm:libkrun containers need Linux with KVM".to_owned());
        }
        Ok(())
    }

    /// One started libkrun container: the VMM child process.
    pub struct LibkrunVm {
        child: TrackedChild,
    }

    impl LibkrunVm {
        /// Provision a minimal guest root (pinned Node.js over the host's
        /// own glibc) and spawn the VMM child that boots the microVM.
        pub fn start(
            container_id: &str,
            container_directory: &Path,
            profile: &str,
            port: u16,
            task: Option<&box_scheduler::TaskContext>,
        ) -> Result<LibkrunVm, String> {
            available()?;
            let vm_dir = vm_dir(container_directory);
            let rootfs = vm_dir.join("rootfs");
            std::fs::create_dir_all(rootfs.join("usr/local"))
                .map_err(|error| format!("cannot create guest root: {error}"))?;

            // Guest Node.js: the same pinned build, extracted straight into
            // the root directory. The marker skips re-provisioning.
            let marker = vm_dir.join("provisioned");
            if !marker.is_file() {
                if let Some(task) = task {
                    task.log("provisioning libkrun guest with Node.js");
                }
                let node_tar = vm_dir.join("node.tar.xz");
                if !node_tar.is_file() {
                    let client = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(300))
                        .build()
                        .map_err(|error| format!("cannot build download client: {error}"))?;
                    let bytes = client
                        .get(guest_node_url())
                        .send()
                        .and_then(|response| response.error_for_status())
                        .and_then(|response| response.bytes())
                        .map_err(|error| format!("cannot download guest Node.js: {error}"))?;
                    std::fs::write(&node_tar, &bytes)
                        .map_err(|error| format!("cannot write {}: {error}", node_tar.display()))?;
                }
                let status = Command::new("tar")
                    .args([
                        "-xJf",
                        &node_tar.to_string_lossy(),
                        "-C",
                        &rootfs.join("usr/local").to_string_lossy(),
                        "--strip-components=1",
                    ])
                    .status()
                    .map_err(|error| format!("cannot extract guest Node.js: {error}"))?;
                if !status.success() {
                    return Err("guest Node.js extraction failed".to_owned());
                }
                copy_host_loader_libs(&rootfs)?;
                fs::write(&marker, super::super::guest_node_url()).map_err(|error| error.to_string())?;
            }

            // The VMM configuration travels as JSON: the child reads it
            // before dlopen, so the daemon keeps no FFI types across the
            // process boundary.
            let config = serde_json::json!({
                "root": rootfs.to_string_lossy(),
                "shares": [
                    { "tag": "dsh", "path": container_directory.join("harness").to_string_lossy() },
                    { "tag": "home", "path": container_directory.join("profile").to_string_lossy() },
                    { "tag": "state", "path": container_directory.join("state").to_string_lossy() },
                ],
                "workdir": "/dsh",
                "exec": "/usr/local/bin/node",
                "args": [
                    "apps/cli/lib/bin.js",
                    "--profile",
                    profile,
                    "--patch",
                    "/state/box-web.patch.yml",
                    "--no-open",
                ],
                "env": {
                    "DSH_HOME": "/home",
                    "DSHBOX_CONTAINER_ID": container_id,
                },
                "port": port,
                "console": container_directory.join("logs").join("host.log").to_string_lossy(),
            });
            let config_path = vm_dir.join("libkrun-cfg.json");
            std::fs::write(
                &config_path,
                serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("cannot write vmm config: {error}"))?;

            let self_exe = std::env::current_exe()
                .map_err(|error| format!("cannot locate this daemon: {error}"))?;
            let child = Command::new(self_exe)
                .args(["--vmm-run", &config_path.to_string_lossy()])
                .spawn()
                .map_err(|error| format!("cannot start VMM child: {error}"))?;
            Ok(LibkrunVm {
                child: TrackedChild::wrap(child),
            })
        }
    }

    impl VmHandle for LibkrunVm {
        fn id(&self) -> Option<u32> {
            self.child.id()
        }

        fn has_exited(&mut self) -> std::io::Result<bool> {
            self.child.try_wait().map(|status| status.is_some())
        }

        fn stop(&mut self) {
            // SIGTERM the VMM; the guest dies with it. No graceful in-guest
            // shutdown channel exists in libkrun 1.x — documented limitation.
            let _ = self.child.kill_tree(false, std::time::Duration::from_secs(2));
        }
    }

    /// Guest dynamic-loader libraries, copied from the host so the guest
    /// Node.js (a normal glibc build) can start. Same-host copy means the
    /// glibc versions match by construction.
    fn copy_host_loader_libs(rootfs: &Path) -> Result<(), String> {
        let lib_dir = PathBuf::from("/lib/x86_64-linux-gnu");
        for name in [
            "ld-linux-x86-64.so.2",
            "libc.so.6",
            "libm.so.6",
            "libgcc_s.so.1",
            "libstdc++.so.6",
            "libdl.so.2",
            "libpthread.so.0",
            "librt.so.1",
        ] {
            let from = lib_dir.join(name);
            if from.is_file() {
                let to = rootfs.join("lib").join(name);
                std::fs::create_dir_all(
                    to.parent().expect("lib has a parent"),
                )
                .map_err(|error| format!("cannot create guest lib dir: {error}"))?;
                std::fs::copy(&from, &to)
                    .map_err(|error| format!("cannot copy {name} into the guest: {error}"))?;
            }
        }
        std::fs::create_dir_all(rootfs.join("lib64"))
            .map_err(|error| format!("cannot create guest lib64: {error}"))?;
        std::fs::copy(
            "/lib64/ld-linux-x86-64.so.2",
            rootfs.join("lib64/ld-linux-x86-64.so.2"),
        )
        .or_else(|_| {
            std::fs::copy(
                rootfs.join("lib/ld-linux-x86-64.so.2"),
                rootfs.join("lib64/ld-linux-x86-64.so.2"),
            )
        })
        .map_err(|error| format!("cannot place the guest loader: {error}"))?;
        Ok(())
    }

    /// The `dshboxd --vmm-run` entry: loads libkrun, configures the VM from
    /// the JSON config, and blocks in `krun_start_enter` for the lifetime
    /// of the microVM.
    pub fn vmm_main(args: &[String]) -> i32 {
        let config_path = match args {
            [path] => PathBuf::from(path),
            _ => {
                eprintln!("usage: dshboxd --vmm-run <config.json>");
                return 2;
            }
        };
        let config: serde_json::Value = match std::fs::read_to_string(&config_path)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
        {
            Ok(value) => value,
            Err(error) => {
                eprintln!("cannot read vmm config: {error}");
                return 2;
            }
        };
        match vmm::run(&config) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("vmm: {error}");
                1
            }
        }
    }

    /// Raw dlopen FFI against libkrun 1.9.x. Loaded lazily so the daemon
    /// itself never needs the library present.
    mod vmm {
        use super::*;
        use libloading::{Library, Symbol};

        const LIB_CANDIDATES: &[&str] = &[
            "libkrun.so.1",
            "libkrun.so",
            "/usr/lib/x86_64-linux-gnu/libkrun.so.1",
            "/usr/local/lib/libkrun.so.1",
        ];

        unsafe fn load() -> Result<Library, String> {
            for candidate in LIB_CANDIDATES {
                if let Ok(library) = Library::new(candidate) {
                    return Ok(library);
                }
            }
            Err("libkrun.so not found; install libkrun (see containers/libkrun)".to_owned())
        }

        /// Panics on a missing symbol are honest: a libkrun too old to have
        /// the 1.9 API cannot run this backend anyway.
        unsafe fn checked(ctx_id: u32, result: i32, what: &str) -> Result<(), String> {
            if result < 0 {
                Err(format!("krun {what} failed ({result})"))
            } else {
                let _ = ctx_id;
                Ok(())
            }
        }

        pub fn run(config: &serde_json::Value) -> Result<(), String> {
            let library = unsafe { load()? };
            unsafe {
                let create: Symbol<unsafe extern "C" fn() -> i32> =
                    library.get(b"krun_create_ctx\0").map_err(|e| e.to_string())?;
                let vm_config: Symbol<unsafe extern "C" fn(u32, u8, u32) -> i32> =
                    library.get(b"krun_set_vm_config\0").map_err(|e| e.to_string())?;
                let set_root: Symbol<unsafe extern "C" fn(u32, *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_set_root\0").map_err(|e| e.to_string())?;
                let add_virtiofs: Symbol<unsafe extern "C" fn(u32, *const std::os::raw::c_char, *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_add_virtiofs\0").map_err(|e| e.to_string())?;
                let set_exec: Symbol<unsafe extern "C" fn(u32, *const std::os::raw::c_char, *const *const std::os::raw::c_char, *const *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_set_exec\0").map_err(|e| e.to_string())?;
                let set_env: Symbol<unsafe extern "C" fn(u32, *const *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_set_env\0").map_err(|e| e.to_string())?;
                let set_workdir: Symbol<unsafe extern "C" fn(u32, *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_set_workdir\0").map_err(|e| e.to_string())?;
                let console: Symbol<unsafe extern "C" fn(u32, *const std::os::raw::c_char) -> i32> =
                    library.get(b"krun_set_console_output\0").map_err(|e| e.to_string())?;
                let start: Symbol<unsafe extern "C" fn(u32) -> i32> =
                    library.get(b"krun_start_enter\0").map_err(|e| e.to_string())?;

                let ctx_id = create();
                if ctx_id < 0 {
                    return Err(format!("krun_create_ctx failed ({ctx_id})"));
                }
                let ctx_id = ctx_id as u32;
                checked(
                    ctx_id,
                    vm_config(ctx_id, 2, 1024),
                    "set_vm_config",
                )?;
                let root = std::ffi::CString::new(
                    config["root"].as_str().unwrap_or(""),
                )
                .map_err(|error| error.to_string())?;
                checked(ctx_id, set_root(ctx_id, root.as_ptr()), "set_root")?;
                for share in config["shares"].as_array().into_iter().flatten() {
                    let tag = std::ffi::CString::new(share["tag"].as_str().unwrap_or(""))
                        .map_err(|error| error.to_string())?;
                    let path = std::ffi::CString::new(share["path"].as_str().unwrap_or(""))
                        .map_err(|error| error.to_string())?;
                    checked(ctx_id, add_virtiofs(ctx_id, tag.as_ptr(), path.as_ptr()), "add_virtiofs")?;
                }
                let console_path = std::ffi::CString::new(config["console"].as_str().unwrap_or(""))
                    .map_err(|error| error.to_string())?;
                checked(ctx_id, console(ctx_id, console_path.as_ptr()), "set_console_output")?;
                let workdir = std::ffi::CString::new(config["workdir"].as_str().unwrap_or("/"))
                    .map_err(|error| error.to_string())?;
                checked(ctx_id, set_workdir(ctx_id, workdir.as_ptr()), "set_workdir")?;
                let exec = std::ffi::CString::new(config["exec"].as_str().unwrap_or(""))
                    .map_err(|error| error.to_string())?;
                let args: Vec<std::ffi::CString> = config["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .map(std::ffi::CString::new)
                    .collect::<Result<_, _>>()
                    .map_err(|error| error.to_string())?;
                let mut arg_ptrs: Vec<*const std::os::raw::c_char> =
                    args.iter().map(|arg| arg.as_ptr()).collect();
                arg_ptrs.push(std::ptr::null());
                let env_pairs = config["env"].as_object().into_iter().flatten();
                let env: Vec<std::ffi::CString> = env_pairs
                    .map(|(key, value)| {
                        std::ffi::CString::new(format!(
                            "{key}={}",
                            value.as_str().unwrap_or_default()
                        ))
                    })
                    .collect::<Result<_, _>>()
                    .map_err(|error| error.to_string())?;
                let mut env_ptrs: Vec<*const std::os::raw::c_char> =
                    env.iter().map(|entry| entry.as_ptr()).collect();
                env_ptrs.push(std::ptr::null());
                checked(
                    ctx_id,
                    set_exec(ctx_id, exec.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()),
                    "set_exec",
                )?;
                let result = start(ctx_id);
                checked(ctx_id, result, "start_enter")
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::{available, LibkrunVm, vmm_main};
