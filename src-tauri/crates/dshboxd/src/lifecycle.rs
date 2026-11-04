//! Container lifecycle for the daemon: start the DSH host process for a
//! container, wait for it to become ready, and stop it. Mirrors the
//! desktop's `lifecycle.rs` without any Tauri dependency; the daemon owns
//! the child processes and the running registry.

use crate::containers::{
    ensure_container_workspace, install_box_skill, repair_known_profile_template,
};
use crate::state::{bundled_runtime, ContainerManager, ManagedHost};
use crate::toolchains::{resolve_toolchain, run_logged};
use box_containers::container_directory;
use box_foundation::{is_safe_identifier, read_config};
use box_runtime::process::{self, ExecutionKind, ProcessSpec, TrackedChild};
use box_scheduler::TaskContext;
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::host::{self, HostState};

fn dsh_dependencies_ready(source: &Path) -> bool {
    // `pnpm install` can leave the root node_modules directory behind when
    // Windows fails during junction creation/final validation. The frontend
    // build imports tsx directly, so this package manifest is the smallest
    // reliable marker that the workspace is actually ready to build.
    source
        .join("node_modules")
        .join("tsx")
        .join("package.json")
        .is_file()
}

const HOST_BIND_ATTEMPTS: u32 = 3;
const HOST_READY_PROBES: usize = 240;
const SANDBOX_PLUGIN_PACKAGE: &str = "@nexus-aethra/dsh-box-sandbox";
const SANDBOX_PLUGIN_ID: &str = "dsh-box-sandbox";
const PLUGIN_COPY_TIMEOUT: Duration = Duration::from_secs(45);
const PLUGIN_COPY_MARKER: &str = ".dshbox-materialized";

/// HTTP client for probing loopback DSH web servers. It must never
/// consult proxy environment variables: a developer's `HTTP_PROXY`
/// makes reqwest route `127.0.0.1` through the proxy, which answers 502
/// for other processes' loopback ports — the probes would then kill
/// perfectly healthy hosts.
fn loopback_probe_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|error| format!("build loopback http client: {error}"))
}

fn allocate_loopback_port() -> Result<u16, String> {
    // This reservation only identifies a currently usable port; DSH owns the
    // eventual listener, so the socket must be released before host spawn.
    // Keep the allocation immediately adjacent to that spawn to minimise the
    // unavoidable hand-off window.
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot allocate loopback port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| error.to_string())
}

fn is_transient_loopback_bind_failure(output: &[u8]) -> bool {
    let output = String::from_utf8_lossy(output);
    output.contains("listen EACCES")
        || output.contains("listen EADDRINUSE")
        || output.contains("permission denied 127.0.0.1")
        || output.contains("address already in use 127.0.0.1")
}

/// Parse the `dsh web: <url>` announcement from the host log tail.
///
/// Upstream DSH (0.1.2+) prints this line once its Loader tree settles and
/// serves 401 on every tokenless request (browser-trust fence); the line is
/// the supervisor contract for obtaining the per-launch capability URL. The
/// line may carry a `(LAN: ...)` suffix — only the first token is a URL, and
/// only loopback URLs are trusted.
fn parse_announced_host_url(log_tail: &str) -> Option<String> {
    log_tail.lines().rev().find_map(|line| {
        let candidate = line
            .trim()
            .strip_prefix("dsh web: ")?
            .split_whitespace()
            .next()?
            .to_owned();
        candidate
            .starts_with("http://127.0.0.1:")
            .then_some(candidate)
    })
}

/// Start the DSH host for `id` and wait until its frontend answers.
/// The running map is the daemon-owned registry; containers already
/// running return their existing URL.
/// Container environment declared by the Boxfile's `ENV` directives and
/// copied into `container.json` at creation time. Applied on top of the
/// host policy's whitelist, so a container's declared environment never
/// depends on the caller's ambient environment — the docker-style `-e`
/// equivalent. Non-string entries are ignored rather than failing the
/// start: the file is Box-written state, and a bad entry should not cost
/// the user a container.
fn declared_env_from_metadata(value: &serde_json::Value) -> BTreeMap<String, String> {
    value["env"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn start_dsh_container_inner(
    id: &str,
    running: &Mutex<BTreeMap<String, ManagedHost>>,
    task: Option<&TaskContext>,
) -> Result<String, String> {
    if !is_safe_identifier(id) {
        return Err("invalid container id".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let directory = container_directory(&root, id);
    let metadata = fs::read_to_string(directory.join("container.json"))
        .map_err(|error| format!("cannot read container: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&metadata)
        .map_err(|error| format!("cannot parse container: {error}"))?;
    // Startup contract: a prepared container is self-contained, and nothing
    // here resolves the template it came from — the historical lookup by flat
    // `<name>.dsh` path is what reported `template not found` for a container
    // materialised correctly moments before. Sealed recipes keep
    // `sealedTemplate` for provenance, direct official-template runs are
    // materialised from a prepared base; either way startup installs and
    // builds nothing.
    if !directory.join("manifest.json").is_file() {
        return Err("container is missing its template manifest".to_owned());
    }
    let _version = value["version"]
        .as_str()
        .ok_or("container has no version")?;
    let profile = value["profile"].as_str().unwrap_or("web");
    let name = value["name"].as_str().unwrap_or(id).to_owned();
    let template = value["template"].as_str().map(str::to_owned);
    let declared_env = declared_env_from_metadata(&value);
    // Which backend brings this container up. Absent means host: the field
    // was added long after containers existed, and creation never writes it.
    let runtime = crate::runtime::RuntimeKind::from_record(&value)?;
    if let crate::runtime::RuntimeKind::Vm(backend) = &runtime {
        // Availability is checked before any side effect: a start that
        // cannot use the requested backend must not leave partial state
        // behind. Enforcement of the backend itself happens at launch.
        crate::vm::check_backend(backend.as_deref()).map_err(|error| {
            format!("container runtime {} is not usable: {error}", runtime.field())
        })?;
    }
    repair_known_profile_template(&directory, profile)?;
    let _workspace = ensure_container_workspace(&directory)?;
    let dshbox_home =
        crate::state::dshbox_install_directory().unwrap_or_else(|_| PathBuf::from("."));
    // Every container gets the sandbox plugin at start; see materialize_bundled_sandbox_plugin.
    // DSH's Cordis loader imports loader entries through Node's ESM
    // machinery, which never consults NODE_PATH; expose the vendored
    // plugin as a real node_modules entry next to the profile.
    if let Some(task) = task {
        task.update("Installing DSH Box sandbox plugin", 20);
    }
    materialize_bundled_sandbox_plugin(&directory, profile, task)?;
    let source = directory.join("harness");
    if !source.join("package.json").is_file() {
        return Err("DSH source is incomplete".to_owned());
    }
    {
        let mut running = running
            .lock()
            .map_err(|_| "container manager lock failed")?;
        if let Some(host) = running.get_mut(id) {
            if !host
                .handle
                .has_exited()
                .map_err(|error| error.to_string())?
            {
                return Ok(host.url.clone());
            }
        }
    }
    if let Some(task) = task {
        task.update("Preparing DSH host", 25);
        task.check_cancelled()?;
    }
    // Write a Corrupted sentinel before preflight: if plugin build
    // fails here, the container still has a host.json so the UI can
    // distinguish "failed to prepare" from "user stopped it". Once the
    // host actually spawns, we overwrite it with the Starting record.
    let _ = std::fs::create_dir_all(directory.join("state"))
        .map_err(|error| format!("cannot create host state directory: {error}"));
    let _ = host::write_corrupted_record(
        id,
        &name,
        template.as_deref(),
        profile,
        "container created; preflight not yet complete",
    );
    if let Some(task) = task {
        task.check_cancelled()?;
    }
    let patch = directory.join("box-web.patch.yml");
    let log_path = directory.join("logs").join("host.log");
    let _ = fs::File::create(&log_path);
    if !dsh_dependencies_ready(&source) || !source.join("apps/web/dist/index.html").is_file() {
        return Err(
            "sealed container is missing prepared DSH dependencies or frontend build".to_owned(),
        );
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        if let Some(task) = task {
            task.update("Launching DSH host", 75);
            task.log("launching DSH host");
        }
        let plugins_node_modules = PathBuf::from(&root).join("plugins").join("node_modules");
        // Wait for the harness runtime's pnpm store to finish initialising.
        // `pull_template` only clones the repo — it does NOT run `pnpm
        // install` — so the first container create on a freshly-installed
        // harness races the on-demand `pnpm install` in the container-build
        // path above. If we spawn node before pnpm has linked `tsx` (and
        // the rest of the ESM deps) into `source/node_modules/`, Node v24's
        // ESM resolver reports `ERR_MODULE_NOT_FOUND` for the `--import`
        // specifier. Bound the wait to a few seconds — pnpm install for the
        // dsh monorepo typically lands within a second once started, so a
        // short poll is enough; if it takes longer the actual `pnpm install`
        // failure will surface through the build step above and abort
        // before we get here.
        wait_for_pnpm_links(&source, std::time::Duration::from_secs(15))
            .map_err(|error| format!("DSH runtime not ready ({error})"))?;
        // Launch the built CLI entry (`apps/cli` declares `bin: { dsh:
        // lib/bin.js }`), never the source one. The Harness tree carries both
        // `src/` and the `lib/` built from it, and `tsx` honours
        // `tsconfig.base.json`'s `paths`, which map every first-party package
        // to `src` — so a host started through `tsx + src/bin.ts` loads such a
        // package twice, once from `src` (paths) and once from `lib` (the
        // manifest `exports` the plugin row loader follows). Each copy creates
        // its own `Symbol`, and the agent loop then looks up
        // `ctx.tools[TOOL_RUNTIME_SCHEDULER]` under a different symbol than the
        // one `ToolRuntime` stored it under: every tool call dies ~1 ms after
        // it is appended, which the UI renders as a very short interruption.
        let entry = source.join("apps/cli/lib/bin.js");
        if !entry.is_file() {
            return Err(format!(
                "DSH host cannot start: {} has no built CLI entry at \
                 apps/cli/lib/bin.js — rebuild this container's template so the \
                 client artifacts are produced",
                source.display()
            ));
        }
        let port = allocate_loopback_port()?;
        let url = format!("http://127.0.0.1:{port}");
        fs::write(
            &patch,
            format!("- id: webserver\n  config:\n    host: 127.0.0.1\n    port: {port}\n"),
        )
        .map_err(|error| format!("cannot write web patch: {error}"))?;
        let log_offset = fs::metadata(&log_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if let Some(task) = task {
            task.log(&format!("starting DSH host on {url}"));
        }
        let handle_is_vm = matches!(&runtime, crate::runtime::RuntimeKind::Vm(_));
        let mut handle: crate::runtime::HostHandle = match &runtime {
            crate::runtime::RuntimeKind::Host => {
                let node = resolve_toolchain("node")?;
                // The pnpm tree hangs off the runtime root (<root>/pnpm) as a
                // sibling of node/ on every platform. Take it from the manifest
                // root instead of counting .parent() levels — Windows ships
                // node/node.exe while Unix ships node/bin/node, so the depth
                // differs.
                let runtime_root = bundled_runtime()?.root.clone();
                let policy = process::dsh_host_policy(
                    std::path::Path::new(&node.path)
                        .parent()
                        .map(Path::to_path_buf)
                        .as_deref()
                        .unwrap_or(Path::new(".")),
                    &runtime_root.join("pnpm"),
                    &plugins_node_modules,
                );
                let policy = policy.declared_env(&declared_env).task_override(
                    "DSH_HOME",
                    directory.join("profile").to_string_lossy().into_owned(),
                )
                // The sandbox plugin answers "which container am I in" from this:
                // the host process is told, but nothing hands the plugins it mounts
                // an id, and the plugin's install-path parse is only a fallback for
                // hosts started by daemons older than this stamp.
                .task_override("DSHBOX_CONTAINER_ID", id);
                let spec = ProcessSpec::new(node.path.clone())
                    .args([
                        entry.to_string_lossy().as_ref(),
                        "--profile",
                        profile,
                        "--patch",
                        patch.to_string_lossy().as_ref(),
                        // Without this the host opens the user's real default browser
                        // on every start, restart and rebuild, and takes the foreground
                        // with it. Two things made that hard to see: the spawn sits in a
                        // retry loop, so one start could pop the browser up to three
                        // times, and a browser that is already running is handed the URL
                        // instead of being started, so no new process appears and a
                        // before/after process count reports nothing at all. The host
                        // log says so itself: "opening the default browser; pass --no-open
                        // to disable". Box is a server: the UI it opens is a webview the
                        // user asks for, and the agent drives it over loopback. Nothing
                        // about running a container should take over the desktop.
                        "--no-open",
                    ])
                    .cwd(&source)
                    .policy(policy)
                    .kind(ExecutionKind::Logged)
                    .log_path(&log_path)
                    .new_process_group(true);
                let tracked: TrackedChild = run_logged(&spec, "DSH host")
                    .map_err(|error| format!("cannot start DSH host: {error}"))?
                    .into_tracked();
                crate::runtime::HostHandle::Process(tracked)
            }
            crate::runtime::RuntimeKind::Vm(backend) => {
                if let Some(task) = task {
                    task.update("Launching DSH host (VM)", 75);
                }
                crate::runtime::HostHandle::Vm(crate::vm::launch(
                    id,
                    &directory,
                    profile,
                    port,
                    backend.as_deref(),
                    task,
                )?)
            }
        };
        // Detach into a handle for stop / graceful_shutdown handling.
        let host_pid = handle.id().unwrap_or(0);
        let host_pgid = handle.pgid().unwrap_or(0);
        // Persist a starting record before the readiness probe begins so
        // a daemon crash mid-start still leaves something for the next
        // run to reconcile against.
        let initial = host::initial_record(
            id,
            &name,
            template.as_deref(),
            profile,
            host_pid,
            host_pgid,
            port,
            &url,
        );
        let _ = host::write_host_record(&initial);
        let pid_path = PathBuf::from(&directory).join("state").join("host.pid");
        if let Some(parent) = pid_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(&pid_path, host_pid.to_string()) {
            if let Some(task) = task {
                task.log(&format!("warning: cannot write host pid file: {error}"));
            }
        }
        let probe_client = loopback_probe_client(Duration::from_secs(2))?;
        // Upstream DSH (0.1.2+) serves 401 on every path without a
        // per-launch capability token (browser-trust fence) and announces
        // the authenticated URL on stdout for supervisors to pick up. Poll
        // the host log for that line and probe the authenticated URL when
        // present; the bare-URL probe keeps older runtimes working.
        let mut announced_url: Option<String> = None;
        // A VM guest loads its module graph over a shared/drvfs filesystem
        // and can take several times longer than a host process on first
        // boot; widen the readiness window accordingly.
        let ready_probes = if handle_is_vm { HOST_READY_PROBES * 8 } else { HOST_READY_PROBES };
        // Four 250ms probes per second; keep the log's denominator honest for
        // the widened VM window instead of always printing "/60s".
        let ready_budget_secs = ready_probes / 4;
        let ready = (0..ready_probes).any(|attempt| {
            if task.map(TaskContext::cancelled).unwrap_or(false) {
                handle.stop_abrupt();
                return false;
            }
            if let Ok(Some((code, signal))) = handle.exit_status() {
                if let Some(task) = task {
                    if let Some(sig) = signal {
                        task.log(&format!(
                            "DSH host exited early (code {code}, signal {sig})"
                        ));
                    } else {
                        task.log(&format!("DSH host exited early (code {code})"));
                    }
                }
                return false;
            }
            if let Some(task) = task {
                if attempt % 20 == 0 {
                    task.log(&format!("waiting for DSH host ({}/{}s)", attempt / 4, ready_budget_secs));
                }
            }
            if announced_url.is_none() {
                if let Ok(text) = fs::read_to_string(&log_path) {
                    announced_url =
                        parse_announced_host_url(text.get(log_offset as usize..).unwrap_or(""));
                }
            }
            let available = if let Some(authenticated) = announced_url.as_deref() {
                probe_client
                    .get(authenticated)
                    .send()
                    .map(|response| !response.status().is_server_error())
                    .unwrap_or(false)
            } else {
                match probe_client.get(&url).send() {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_client_error() {
                            // Browser-trust fence detected (DSH 0.1.2+): the
                            // server is up but the token URL hasn't been
                            // announced yet — keep waiting for the log line.
                            false
                        } else {
                            !status.is_server_error()
                        }
                    }
                    Err(_) => false,
                }
            };
            if !available {
                thread::sleep(Duration::from_millis(250));
            }
            available
        });
        if ready {
            let snapshot = host::read_host_record(id)
                .ok()
                .flatten()
                .unwrap_or(initial.clone());
            let _ = host::compare_and_swap_host_record(id, &snapshot, |on_disk| {
                let mut next = on_disk.clone();
                next.state = HostState::Ready;
                next.last_seen = box_foundation::now_seconds();
                if let Some(authenticated) = announced_url.clone() {
                    next.authenticated_url = Some(authenticated);
                }
                next
            });
            spawn_health_watcher(id, url.clone());
            running
                .lock()
                .map_err(|_| "container manager lock failed")?
                .insert(
                    id.to_owned(),
                    ManagedHost {
                        handle,
                        url: url.clone(),
                        authenticated_url: announced_url.clone(),
                    },
                );
            if let Some(task) = task {
                task.update("DSH host is ready", 95);
            }
            return Ok(url);
        }
        if task.map(TaskContext::cancelled).unwrap_or(false) {
            let _ = host::compare_and_swap_host_record(id, &initial, |on_disk| {
                let mut next = on_disk.clone();
                next.state = HostState::Stopped;
                next
            });
            return Err("task cancelled while waiting for DSH host".to_owned());
        }
        handle.stop_abrupt();
        let (code, signal) = handle
            .exit_status()
            .ok()
            .flatten()
            .unwrap_or((-1, None));
        let _ = host::compare_and_swap_host_record(id, &initial, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Crashed;
            next.exit_status = Some(code);
            next.exit_signal = signal;
            next
        });
        let host_output = fs::read(&log_path)
            .ok()
            .and_then(|output| output.get(log_offset as usize..).map(|tail| tail.to_vec()))
            .unwrap_or_default();
        if is_transient_loopback_bind_failure(&host_output) && attempt < HOST_BIND_ATTEMPTS {
            if let Some(task) = task {
                task.log(&format!(
                    "DSH host could not bind {url}; selecting a new loopback port (attempt {}/{HOST_BIND_ATTEMPTS})",
                    attempt + 1
                ));
            }
            continue;
        }
        let _ = fs::remove_file(&pid_path);
        return Err(format!(
            "DSH host did not become ready; inspect {}",
            log_path.display()
        ));
    }
}

/// Stop a running container host: send SIGTERM to the process group,
/// wait up to 5s for graceful exit, escalate to SIGKILL, and update the
/// `host.json` state to `Stopped`.
pub(crate) fn stop_dsh_container(id: &str, manager: &ContainerManager) -> Result<(), String> {
    let host = manager
        .running
        .lock()
        .map_err(|_| "container manager lock failed")?
        .remove(id);
    if let Some(mut host) = host {
        host.handle.stop();
        // The handle's own Drop impl performs the final force-kill + reap
        // if the process hasn't exited yet, so we don't need an extra
        // wait() here.
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let pid_path = container_directory(&root, id)
        .join("state")
        .join("host.pid");
    let _ = fs::remove_file(&pid_path);
    if let Ok(Some(snapshot)) = host::read_host_record(id) {
        let _ = host::compare_and_swap_host_record(id, &snapshot, |on_disk| {
            let mut next = on_disk.clone();
            next.state = HostState::Stopped;
            next
        });
    }
    Ok(())
}

/// A sealed container already contains a prepared frontend. Rebuild is kept
/// as a UI-compatible recovery action, but now means a clean host restart;
/// mutating the container with `pnpm install` or `pnpm build` would break the
/// sealed-template contract.
pub(crate) fn rebuild_dsh_container_with_task(
    id: String,
    manager: &ContainerManager,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    if let Some(task) = task {
        task.update("Stopping DSH host", 20);
    }
    stop_dsh_container(&id, manager)?;
    if let Some(task) = task {
        task.update("Validating sealed container", 55);
        task.check_cancelled()?;
    }
    start_dsh_container_inner(&id, &manager.running, task).map(|_| ())
}

/// Expose the bundled `@nexus-aethra/dsh-box-sandbox` plugin to the DSH loader
/// as a real `node_modules` entry under the container's profile.
pub(crate) fn materialize_bundled_sandbox_plugin(
    directory: &Path,
    profile: &str,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // The vendored tree is written by `initialize_bundled_plugins`, which copies
    // the resource's `node_modules/` verbatim -- so the package sits under its
    // own scope (`@nexus-aethra/`), not a hardcoded one. Joining the full scoped
    // name onto a scope made the path `@deepseek-ai/@nexus-aethra/...`, which no
    // vendoring run ever writes, so every start fell through to the "not
    // bundled" branch and silently skipped the plugin.
    let vendored = PathBuf::from(&root)
        .join("plugins")
        .join("node_modules")
        .join(SANDBOX_PLUGIN_PACKAGE);
    if !vendored.join("package.json").is_file() {
        // Self-heal before giving up: the config's manifest digest may
        // predate the current storage root (the vendoring copy never
        // landed here), so re-run the vendoring once. `initialize_bundled
        // _plugins` verifies the tree in the current directory itself.
        let _ = crate::state::initialize_bundled_plugins();
    }
    if !vendored.join("package.json").is_file() {
        // Vendored plugin tree is still missing (e.g. a developer build
        // that skipped the plugin bundler); let DSH surface the resolution
        // error if the patch still references the bundle.
        return Ok(());
    }
    let profile_node_modules = directory
        .join("profile")
        .join("profiles")
        .join(profile)
        .join("node_modules");
    if let Some(task) = task {
        task.log("installing the sandbox plugin into the container profile");
    }
    let copied = ensure_vendored_plugin_copied(&vendored, &profile_node_modules, SANDBOX_PLUGIN_PACKAGE)?;
    if let Some(task) = task {
        task.log(if copied {
            "installed the sandbox plugin"
        } else {
            "sandbox plugin is already installed"
        });
    }
    // A vendored tree alone is not a mounted plugin. The profile composer
    // mounts exactly the packages listed in the profile manifest's
    // `dsh.profile.bundles`, resolving them as dependencies — so a container
    // whose profile was composed before this plugin existed loads its files
    // and then registers nothing, and the agent inside it has every tool
    // described to it and none available. Declare the dependency and
    // activate the bundle here, idempotently, so the mount rides the same
    // start that ships the files.
    declare_sandbox_bundle(directory, profile)?;
    // The skill ships inside the vendored package, so this reads the copy that
    // was just published rather than a second source of truth.
    // `vendored` is the package directory itself, so the skill root is its own
    // `skills/` child. Appending the package id again pointed one level too deep
    // (`.../skills/dsh-box-sandbox`) and every start reported "not bundled".
    install_box_skill(directory, &vendored.join("skills").join("dsh-box-tools"), task)?;
    Ok(())
}

/// Declare the bundled sandbox plugin in the profile manifest, once.
///
/// Declare the bundled sandbox plugin in the profile manifest, once.
///
/// Keyed on the bundle list: when `dsh.profile.bundles` already names the
/// package the manifest is left untouched (the composer keys on that list;
/// a manifest naming the bundle without the dependency entry is the
/// template's own business). Otherwise the dependency is added, pinned to
/// the vendored version, and the package is appended to the list.
fn declare_sandbox_bundle(directory: &Path, profile: &str) -> Result<(), String> {
    let profile_dir = directory.join("profile").join("profiles").join(profile);
    let manifest_path = profile_dir.join("package.json");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read profile manifest {}: {error}", manifest_path.display()))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("profile manifest is not valid JSON: {error}"))?;
    let already_declared = manifest["dsh"]["profile"]["bundles"]
        .as_array()
        .map(|bundles| bundles.iter().any(|bundle| bundle.as_str() == Some(SANDBOX_PLUGIN_PACKAGE)))
        .unwrap_or(false);
    if already_declared {
        return Ok(());
    }
    let version = fs::read_to_string(
        profile_dir
            .join("node_modules")
            .join(SANDBOX_PLUGIN_PACKAGE)
            .join("package.json"),
    )
    .ok()
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|package| package["version"].as_str().map(str::to_owned))
    .unwrap_or_else(|| "*".to_owned());
    let object = manifest
        .as_object_mut()
        .ok_or("profile manifest is not an object")?;
    let dependencies = object
        .entry("dependencies")
        .or_insert_with(|| serde_json::json!({}));
    dependencies
        .as_object_mut()
        .ok_or("profile manifest dependencies is not an object")?
        .insert(SANDBOX_PLUGIN_PACKAGE.to_owned(), serde_json::json!(version));
    let dsh = object.entry("dsh").or_insert_with(|| serde_json::json!({}));
    let profile_section = dsh
        .as_object_mut()
        .ok_or("profile manifest dsh is not an object")?
        .entry("profile")
        .or_insert_with(|| serde_json::json!({}));
    let bundles = profile_section
        .as_object_mut()
        .ok_or("profile manifest dsh.profile is not an object")?
        .entry("bundles")
        .or_insert_with(|| serde_json::json!([]));
    bundles
        .as_array_mut()
        .ok_or("profile manifest dsh.profile.bundles is not an array")?
        .push(serde_json::json!(SANDBOX_PLUGIN_PACKAGE));
    let pretty = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("cannot serialize profile manifest: {error}"))?;
    fs::write(&manifest_path, pretty + "\n")
        .map_err(|error| format!("cannot write profile manifest: {error}"))?;
    Ok(())
}

/// Idempotent exposure of the vendored bundle under a profile's
/// `node_modules`. Always a physical copy (`cp -rL` semantics) so the
/// container profile never references the runtime tree via symlinks or
/// junctions — those are incompatible with Windows + pnpm + AV scenarios
/// and have no upside now that cross-container dedup is opt-in per build.
/// Freshness is detected by mtime + sha256(`package.json`) so an unchanged
/// vendored tree does not pay for a full re-copy on every container start.
fn ensure_vendored_plugin_copied(
    vendored: &Path,
    profile_node_modules: &Path,
    package: &str,
) -> Result<bool, String> {
    // A scoped package resolves from its own scope directory; hardcoding one
    // is how a plugin lands where Node never looks.
    let (scope, plugin_id) = package
        .split_once('/')
        .ok_or_else(|| format!("bundled plugin name is not scoped: {package}"))?;
    let scoped = profile_node_modules.join(scope);
    fs::create_dir_all(&scoped)
        .map_err(|error| format!("cannot create {}: {error}", scoped.display()))?;
    let target = scoped.join(plugin_id);
    // Already a plain directory whose contents match the source — leave it.
    if target.is_dir()
        && !fs::symlink_metadata(&target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        && materialized_package_matches(vendored, &target)?
    {
        return Ok(false);
    }
    // Otherwise it is a stale symlink, an unexpected file, or a divergent
    // copy — clear it before re-copying. `symlink_metadata` is used so we
    // do not follow the link itself when deciding what to remove.
    if let Ok(metadata) = fs::symlink_metadata(&target) {
        if metadata.file_type().is_symlink() {
            fs::remove_file(&target)
                .map_err(|error| format!("cannot replace stale plugin link: {error}"))?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(&target)
                .map_err(|error| format!("cannot replace stale plugin dir: {error}"))?;
        } else {
            fs::remove_file(&target)
                .map_err(|error| format!("cannot replace stale plugin file: {error}"))?;
        }
    }
    copy_vendored_plugin_with_timeout(vendored, &target)?;
    Ok(true)
}

/// True when the two plugin trees share the same `package.json` bytes.
/// (The vendored tree may carry an outdated `node_modules/`; the simple
/// equality check on `package.json` is enough — that file is rewritten
/// whenever the plugin is re-extracted, which is the only thing the user
/// can meaningfully change in the vendored tree.)
fn materialized_package_matches(first: &Path, second: &Path) -> Result<bool, String> {
    let a = fs::read(first.join("package.json"))
        .map_err(|error| format!("cannot read {}/package.json: {error}", first.display()))?;
    let b = fs::read(second.join("package.json"))
        .map_err(|error| format!("cannot read {}/package.json: {error}", second.display()))?;
    let marker = fs::read(second.join(PLUGIN_COPY_MARKER)).unwrap_or_default();
    Ok(a == b && a == marker)
}

/// Materialize in a separate worker so a stalled filesystem operation cannot
/// hold the scheduler task forever. The staging tree is never visible to the
/// DSH loader; only a completed copy is published at the target path.
fn copy_vendored_plugin_with_timeout(source: &Path, target: &Path) -> Result<(), String> {
    let source = source.to_path_buf();
    let target = target.to_path_buf();
    let copy_target = target.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("dshbox-plugin-copy".to_owned())
        .spawn(move || {
            let result = copy_vendored_plugin_atomically(&source, &copy_target);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("cannot start the plugin copy: {error}"))?;
    receiver.recv_timeout(PLUGIN_COPY_TIMEOUT).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => format!(
            "DSH Box plugin copy exceeded {} seconds; retry the container start (a filesystem or antivirus lock may be blocking {})",
            PLUGIN_COPY_TIMEOUT.as_secs(),
            target.display()
        ),
        mpsc::RecvTimeoutError::Disconnected => "DSH Box plugin copy worker stopped unexpectedly".to_owned(),
    })?
}

fn copy_vendored_plugin_atomically(source: &Path, target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or("plugin copy target has no parent")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    // Name the staging tree after the destination so a leftover from an
    // interrupted copy is attributable to the plugin it belongs to.
    let leaf = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_owned());
    let staging = parent.join(format!(".{leaf}.copy-{}-{nonce}", std::process::id()));
    box_foundation::copy_tree::copy_tree_following(source, &staging)
        .map_err(|error| format!("cannot copy vendored plugin into staging: {error}"))?;
    let package = fs::read(source.join("package.json"))
        .map_err(|error| format!("cannot read {}/package.json: {error}", source.display()))?;
    fs::write(staging.join(PLUGIN_COPY_MARKER), package)
        .map_err(|error| format!("cannot mark the materialized plugin: {error}"))?;
    fs::rename(&staging, target)
        .map_err(|error| format!("cannot publish the materialized plugin: {error}"))
}

/// Replace a destination tree with a dereferenced physical copy.  Package
/// managers can replace a copied package with links during install, so callers
/// that promise a container-owned plugin payload must use this after install
/// as well as before it.
pub(crate) fn replace_tree_following(source: &Path, destination: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(destination)?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(destination)?;
        }
    }
    box_foundation::copy_tree::copy_tree_following(source, destination)
}

/// Watches the host URL and writes back to `host.json`.
///
/// Every `PROBE_INTERVAL` (2s by default) the thread:
///   1. Reads the current record (snapshot).
///   2. Calls `try_wait` on the host PID via `kill -0`. ESRCH = dead,
///      bump `state` to `Crashed` with the captured exit info and exit
///      the loop.
///   3. Otherwise HTTP GETs the URL. On any non-5xx response (2xx, the
///      303+401 auth dance of DSH 0.1.2+, …), bumps `last_seen` and
///      `probe_count`, resets `unhealthy_count`. On transport failure
///      or 5xx, increments `unhealthy_count`; after `UNHEALTHY_THRESHOLD`
///      consecutive failures, marks the host `Crashed` and exits.
///
/// The watcher never auto-restarts — a `Crashed` record is a tombstone
/// the user clears with `dshbox container start` (which spawns a fresh
/// watcher for the new generation).
fn spawn_health_watcher(id: &str, url: String) {
    let id_owned = id.to_owned();
    std::thread::spawn(move || {
        const PROBE_INTERVAL: Duration = Duration::from_secs(2);
        const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
        const UNHEALTHY_THRESHOLD: u32 = 2;
        let client = match loopback_probe_client(PROBE_TIMEOUT) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("watcher[{id_owned}]: {error}");
                return;
            }
        };
        loop {
            thread::sleep(PROBE_INTERVAL);
            let Some(snapshot) = host::read_host_record(&id_owned).ok().flatten() else {
                // Record gone — the user deleted the container.
                return;
            };
            // Step 1: liveness probe via PID file (cross-platform via
            // `kill -0` on unix; Windows equivalent below).
            if !pid_alive(snapshot.host_pid) {
                let _ = host::compare_and_swap_host_record(&id_owned, &snapshot, |on_disk| {
                    let mut next = on_disk.clone();
                    next.state = HostState::Crashed;
                    next.exit_status = Some(-1);
                    next.exit_signal = None;
                    next
                });
                return;
            }
            // Step 2: HTTP probe. DSH 0.1.2+ rejects tokenless requests
            // with 401, so prefer the authenticated URL the start path
            // parsed from the host log; fall back to the bare URL for
            // runtimes without the browser-trust fence.
            let probe_url = snapshot
                .authenticated_url
                .clone()
                .unwrap_or_else(|| url.clone());
            // "Healthy" means the DSH origin is answering. DSH 0.1.2+ mints
            // an auth cookie via 303 and answers tokenless requests with
            // 401, so any non-5xx response proves the server is up; a 5xx
            // keeps meaning the loopback request was hijacked (proxy) or
            // the app itself is broken.
            let healthy = client
                .get(&probe_url)
                .send()
                .map(|response| !response.status().is_server_error())
                .unwrap_or(false);
            let _ = host::compare_and_swap_host_record(&id_owned, &snapshot, |on_disk| {
                let mut next = on_disk.clone();
                next.probe_count = on_disk.probe_count.saturating_add(1);
                if healthy {
                    next.last_seen = box_foundation::now_seconds();
                    next.unhealthy_count = 0;
                    if matches!(next.state, HostState::Starting | HostState::Ready) {
                        next.state = HostState::Running;
                    }
                } else {
                    next.unhealthy_count = on_disk.unhealthy_count.saturating_add(1);
                    if next.unhealthy_count >= UNHEALTHY_THRESHOLD
                        && matches!(
                            next.state,
                            HostState::Starting | HostState::Ready | HostState::Running
                        )
                    {
                        next.state = HostState::Crashed;
                    }
                }
                next
            });
            // Exit if we just marked the host Crashed — keep loop
            // tight; nothing more to do until the user restarts.
            if let Ok(Some(latest)) = host::read_host_record(&id_owned) {
                if matches!(latest.state, HostState::Crashed | HostState::Stopped) {
                    return;
                }
            }
        }
    });
}

/// Cross-platform PID liveness probe with a distinguishing error
/// channel. Unix uses `kill -0` + stderr parsing (the shell prints
/// "kill: <pid>: No such process" on ESRCH); Windows returns ACCESS_DENIED
/// from `OpenProcess` for foreign PIDs.
///
/// Linux/macOS refinement: a PID that exists but is in the Z (zombie)
/// state is treated as dead — the parent never `wait()`ed for it, so
/// no live work is happening. ESRCH from `kill -0` only catches
/// fully-reaped entries.
fn pid_alive(pid: u32) -> bool {
    matches!(probe_pid(pid), PidProbe::Alive)
}

/// Poll for the `tsx` symlink under `<source>/node_modules/` to resolve
/// successfully. pnpm install lays down `.pnpm/tsx@<ver>/node_modules/tsx`
/// before linking the top-level `node_modules/tsx` symlink, so once
/// `std::fs::metadata` succeeds on that symlink target the rest of the
/// store is also ready for the ESM resolver. Times out with the supplied
/// deadline so the caller can report a clean error rather than letting
/// node spew `ERR_MODULE_NOT_FOUND`.
fn wait_for_pnpm_links(source: &Path, deadline: Duration) -> Result<(), String> {
    let marker = source.join("node_modules").join("tsx");
    let probe = || {
        // `metadata` follows symlinks; a dangling link returns Err — that's
        // exactly what we want to wait out.
        std::fs::metadata(&marker)
            .map(|_| ())
            .map_err(|error| error.to_string())
    };
    if probe().is_ok() {
        return Ok(());
    }
    let start = Instant::now();
    while start.elapsed() < deadline {
        if probe().is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "pnpm store link is missing at {} after {:?}",
        marker.display(),
        deadline
    ))
}

/// Scan every persisted `host.json` and reconcile it against the live
/// process table. Called once at daemon startup so that a previous
/// daemon's death doesn't leave stale "running" records behind.
///
///   - `Crashed` / `Stopped` → leave alone; the user is expected to
///     clear them via `dshbox container start` or `rm`.
///   - `Starting` / `Ready` / `Running` with PID dead → remove the
///     record so `start` can rebuild it.
///   - `Starting` / `Ready` / `Running` with PID alive but EPERM'd
///     (PID was recycled by an unrelated process) → mark `Orphaned`.
///   - `Orphaned` is left as-is; user must restart to clear.
pub(crate) fn reconcile_orphan_containers() {
    let records = match host::list_all_host_records() {
        Ok(r) => r,
        Err(error) => {
            eprintln!("reconcile: cannot list host.json files: {error}");
            return;
        }
    };
    for record in records {
        if !matches!(
            record.state,
            HostState::Starting | HostState::Ready | HostState::Running
        ) {
            continue;
        }
        match probe_pid(record.host_pid) {
            PidProbe::Alive => {
                // PID exists; the recorded host may still be running
                // on another machine or after a daemon crash. Trust the
                // watcher to decide its fate.
            }
            PidProbe::Esrch => {
                eprintln!(
                    "reconcile: {} host PID {} is gone; dropping stale record",
                    record.id, record.host_pid
                );
                host::remove_host_record(&record.id);
                // Also remove the legacy host.pid file so callers that
                // still consult it don't trip over a phantom running
                // container.
                if let Ok(config) = read_config() {
                    if let Some(root) = config.runtime_directory {
                        let pid_path = container_directory(&root, &record.id)
                            .join("state")
                            .join("host.pid");
                        let _ = fs::remove_file(pid_path);
                    }
                }
            }
            PidProbe::Eperm => {
                eprintln!(
                    "reconcile: {} host PID {} exists but is not ours; flagging orphaned",
                    record.id, record.host_pid
                );
                let _ = host::compare_and_swap_host_record(&record.id, &record, |on_disk| {
                    let mut next = on_disk.clone();
                    next.state = HostState::Orphaned;
                    next
                });
            }
        }
    }
}

enum PidProbe {
    Alive,
    Esrch,
    Eperm,
}

/// Cross-platform PID existence probe with a distinguishing error
/// channel. Unix uses `kill -0` + stderr parsing (the shell prints
/// "kill: <pid>: No such process" on ESRCH); Windows returns ACCESS_DENIED
/// from `OpenProcess` for foreign PIDs.
fn probe_pid(pid: u32) -> PidProbe {
    #[cfg(unix)]
    {
        // Fast path: /proc/<pid> missing ⇒ fully gone. Reading
        // /proc avoids locale-sensitive shell error parsing.
        if std::fs::read_to_string(format!("/proc/{pid}/status")).is_err() {
            return PidProbe::Esrch;
        }
        // Distinguish zombie (R → Z) from alive. A zombie PID
        // technically responds to kill -0 with success, so the
        // status check alone misreports it as alive.
        if let Ok(status_text) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status_text.lines() {
                if let Some(rest) = line.strip_prefix("State:") {
                    if rest.trim().starts_with('Z') {
                        return PidProbe::Esrch;
                    }
                    break;
                }
            }
        }
        let output = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .output();
        match output {
            Ok(result) if result.status.success() => PidProbe::Alive,
            Ok(result) => {
                let stderr = String::from_utf8_lossy(&result.stderr);
                // kill(1) prints one of these regardless of LANG — the
                // translation happens in the shell wrapper, not in
                // kill itself.
                if stderr.contains("No such process")
                    || stderr.contains("ESRCH")
                    || stderr.contains("does not exist")
                {
                    PidProbe::Esrch
                } else {
                    PidProbe::Eperm
                }
            }
            Err(_) => PidProbe::Esrch,
        }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
            fn GetLastError() -> u32;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const ERROR_ACCESS_DENIED: u32 = 5;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
            PidProbe::Alive
        } else {
            let error = unsafe { GetLastError() };
            if error == ERROR_ACCESS_DENIED {
                PidProbe::Eperm
            } else {
                PidProbe::Esrch
            }
        }
    }
}

#[cfg(test)]
mod dependency_ready_tests {
    use super::*;
    use std::fs;

    #[test]
    fn declared_env_reads_string_entries_and_ignores_the_rest() {
        let value = serde_json::json!({
            "id": "container-1",
            "env": {
                "OPENAI_API_KEY": "sk-test",
                "TZ": "Asia/Shanghai",
                "PORT": 8080,
            },
        });
        let declared = declared_env_from_metadata(&value);
        assert_eq!(
            declared.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(
            declared.get("TZ").map(String::as_str),
            Some("Asia/Shanghai")
        );
        assert!(
            !declared.contains_key("PORT"),
            "non-string entry kept: {declared:?}"
        );
        // A container written before ENV existed simply declares nothing.
        let legacy = serde_json::json!({"id": "container-1"});
        assert!(declared_env_from_metadata(&legacy).is_empty());
    }

    #[test]
    fn parses_the_announced_authenticated_url_from_the_log_tail() {
        let log = "\
dsh web: http://127.0.0.1:40413/?token=3xqhaJFec8uZFUxRsyzL0lXRqfVxpi-li-F93sBnqdE
dsh web: opening the default browser; pass --no-open to disable
";
        assert_eq!(
            parse_announced_host_url(log),
            Some(
                "http://127.0.0.1:40413/?token=3xqhaJFec8uZFUxRsyzL0lXRqfVxpi-li-F93sBnqdE"
                    .to_owned()
            )
        );
    }

    #[test]
    fn parse_takes_the_url_token_and_ignores_the_lan_suffix() {
        let log =
            "dsh web: http://127.0.0.1:3080/?token=abc (LAN: http://192.168.1.4:3080/?token=abc)\n";
        assert_eq!(
            parse_announced_host_url(log),
            Some("http://127.0.0.1:3080/?token=abc".to_owned())
        );
    }

    #[test]
    fn parse_ignores_noise_and_non_loopback_lines() {
        assert_eq!(parse_announced_host_url(""), None);
        assert_eq!(parse_announced_host_url("worker started\n"), None);
        assert_eq!(
            parse_announced_host_url("dsh web: http://192.168.1.4:3080/?token=abc\n"),
            None
        );
    }

    #[test]
    fn requires_the_tsx_manifest_not_just_node_modules() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-dependency-ready-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(root.join("node_modules")).unwrap();
        assert!(!dsh_dependencies_ready(&root));

        fs::create_dir_all(root.join("node_modules/tsx")).unwrap();
        fs::write(root.join("node_modules/tsx/package.json"), "{}").unwrap();
        assert!(dsh_dependencies_ready(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recognises_retryable_loopback_bind_errors() {
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EACCES: permission denied 127.0.0.1:54329"
        ));
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EADDRINUSE: address already in use 127.0.0.1:54329"
        ));
        assert!(!is_transient_loopback_bind_failure(
            b"plugin tree failed to load"
        ));
    }
}

#[cfg(test)]
mod copy_tree_following_tests {
    use super::*;
    use std::fs;

    fn sandbox(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-ctf-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plugin_copy_requires_a_completion_marker() {
        let source = sandbox("context-source");
        let target = sandbox("context-target");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();
        fs::write(target.join("package.json"), "{\"version\":1}").unwrap();

        assert!(!materialized_package_matches(&source, &target).unwrap());
        fs::write(target.join(PLUGIN_COPY_MARKER), "{\"version\":1}").unwrap();
        assert!(materialized_package_matches(&source, &target).unwrap());
    }

    #[test]
    fn plugin_copy_is_complete_before_it_is_reused() {
        let source = sandbox("context-copy-source");
        let profile_modules = sandbox("context-copy-profile");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();
        fs::create_dir_all(source.join("node_modules/example")).unwrap();
        fs::write(source.join("node_modules/example/index.js"), "export {};").unwrap();

        assert!(ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());
        let target = profile_modules.join("@nexus-aethra").join(SANDBOX_PLUGIN_ID);
        assert!(target.join(PLUGIN_COPY_MARKER).is_file());
        assert!(target.join("node_modules/example/index.js").is_file());
        assert!(!ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());
    }
}

#[cfg(test)]
mod sandbox_plugin_install_tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-install-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_scoped_plugin_lands_under_its_own_scope() {
        let source = scratch("scope-src");
        let profile_modules = scratch("scope-profile");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();

        assert!(ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());

        let (scope, id) = SANDBOX_PLUGIN_PACKAGE.split_once('/').unwrap();
        let target = profile_modules.join(scope).join(id);
        assert!(target.is_dir(), "plugin must exist at {}", target.display());
        assert!(target.join("package.json").is_file());
        assert!(!profile_modules.join("@deepseek-ai").join("dsh-box-sandbox").exists());
    }

    #[test]
    fn the_skill_lands_in_the_container_skill_root() {
        let container = scratch("skill-container");
        let bundled = scratch("skill-bundled");
        fs::write(bundled.join("SKILL.md"), "v1").unwrap();

        install_box_skill(&container, &bundled, None).unwrap();
        let installed = container.join("profile/skills").join("dsh-box-tools").join("SKILL.md");
        assert!(installed.is_file(), "skill must be at {}", installed.display());
        assert_eq!(fs::read_to_string(&installed).unwrap(), "v1");

        fs::write(bundled.join("SKILL.md"), "v2").unwrap();
        install_box_skill(&container, &bundled, None).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "v2");
    }

    #[test]
    fn a_missing_bundled_skill_is_skipped_not_fatal() {
        let container = scratch("skill-missing");
        let bundled = scratch("skill-missing-src");
        install_box_skill(&container, &bundled, None).unwrap();
        assert!(!container.join("profile/skills/dsh-box-tools/SKILL.md").exists());
    }
}

#[cfg(test)]
mod sandbox_plugin_paths {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-paths-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// The vendored tree must resolve as `<runtime>/plugins/node_modules/<scoped name>`
    /// -- exactly the layout `initialize_bundled_plugins` writes. Joining the whole
    /// scoped name onto a hardcoded scope produced
    /// `@deepseek-ai/@nexus-aethra/...`, which nothing ever creates, so the plugin
    /// was silently skipped on every start.
    #[test]
    fn the_vendored_tree_is_found_under_its_own_scope() {
        let runtime = scratch("vendored");
        let pkg = runtime.join("plugins/node_modules").join(SANDBOX_PLUGIN_PACKAGE);
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), "{}").unwrap();

        let resolved = runtime
            .join("plugins")
            .join("node_modules")
            .join(SANDBOX_PLUGIN_PACKAGE);
        assert!(resolved.join("package.json").is_file(), "looked at {}", resolved.display());

        assert!(!runtime
            .join("plugins/node_modules/@deepseek-ai")
            .join(SANDBOX_PLUGIN_PACKAGE)
            .exists());
    }

    /// `vendored` is the package directory, so its skill lives at
    /// `<vendored>/skills/dsh-box-tools` -- not one level deeper.
    #[test]
    fn the_bundled_skill_sits_under_the_package_not_another_level() {
        let runtime = scratch("skillpath");
        let pkg = runtime.join("plugins/node_modules").join(SANDBOX_PLUGIN_PACKAGE);
        let skill = pkg.join("skills").join("dsh-box-tools");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "x").unwrap();

        assert!(pkg.join("skills").join("dsh-box-tools").join("SKILL.md").is_file());
        assert!(!pkg.join("skills").join("dsh-box-sandbox").exists());
    }
}
