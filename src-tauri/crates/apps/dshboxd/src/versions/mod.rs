//! DSH version management for the daemon: catalog refresh, install, and
//! uninstall. Mirrors the desktop's `versions.rs` + `commands/versions.rs`
//! without any Tauri dependency.
//!
//! Templates are the single source of truth: harness clones are mirrored
//! into the template index by `pull_template`, and the Harness tab in the
//! UI is a derived view over that index (no separate `dsh-catalog.json`
//! or legacy "base template" writer any more). The remote tag list is
//! fetched on demand for the "Load versions" button — it is never
//! persisted.

use box_dsh_versions::{
    installed_versions, parse_template_ref, read_template_index, sort_catalog, DshVersion,
    HARNESS_STANDARD_REF,
};
use box_foundation::{
    atomic_write_json, mirror_url, now_seconds, read_config, write_config, write_template_manifest,
    PublishedResourceKind, RuntimeLayout, TemplateManifest, STORAGE_SCHEMA_VERSION,
};
use box_runtime::{
    mirror::{MirrorCallbacks, MirrorError},
    process::{ExecutionKind, ProcessSpec},
};
use box_scheduler::TaskContext;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::info;

use crate::{
    state::bundled_runtime,
    toolchains::{
        pnpm_network_failure_hint, pnpm_policy, resolve_toolchain, run_logged, TaskCancel,
    },
};

mod prepared_base_index;
mod dsh_catalog;

// prepared_base_index is the storage, dsh_catalog is the remote catalogue, and
// this file is the orchestration that turns one into the other. The globs
// below keep every existing `crate::versions::x` path working.
pub(crate) use prepared_base_index::*;
use prepared_base_index::{read_prepared_base_index, write_prepared_base_index};
pub(crate) use dsh_catalog::*;














/// Pull and prepare a root Harness template. The base is complete: it installs
/// dependencies and builds the client artifacts (native addon, host/client
/// libraries, web frontend) once per Harness version, so every template built
/// from it and every container created from those templates is a copy rather
/// than another build. The build does not depend on a profile's plugins, which
/// is what makes sharing it across templates sound.
pub(crate) fn pull_template_with_cancel(
    ref_value: String,
    cancelled: impl Fn() -> bool + Send + 'static,
    task: &TaskContext,
) -> Result<(), String> {
    // The cancel flag is asked more than once: the clone consumes it, and the
    // error path needs it again to tell a user cancellation from a failure.
    let cancel_flag = std::sync::Arc::new(std::sync::Mutex::new(cancelled));
    let cancelled = {
        let flag = std::sync::Arc::clone(&cancel_flag);
        move || flag.lock().map(|guard| guard()).unwrap_or(true)
    };
    let root = read_config()?
        .runtime_directory
        .clone()
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let parsed = parse_template_ref(&ref_value)
        .map_err(|error| format!("invalid harness reference: {error}"))?;
    let mut index = read_prepared_base_index(&layout)?;
    if index.contains_key(&parsed.version) {
        return Err(format!(
            "prepared Harness base already exists for {}",
            parsed.version
        ));
    }

    // Declare the whole shape up front so the UI can show what is still to
    // come, not just what is happening now. Budgets come from measurement:
    // `pnpm run build` takes 433-471 s on this machine, which is most of the
    // elapsed time, so the three build sub-steps carry most of the budget.
    task.plan_stages(&[
        // Measured after the shared mirror landed (2026-11-05): a first
        // version costs ~10 s to fetch and ~60 s to check out, a repeat costs
        // nothing but the checkout.
        ("clone", "Fetching Harness source from the shared mirror", 75),
        ("install", "Installing DSH dependencies", 120),
        ("build", "Building DSH client artifacts", 450),
        ("validate", "Validating prepared base", 15),
        ("publish", "Publishing prepared base", 40),
    ]);
    task.stage_started("clone");
    // Which stage is running right now, so every error path can mark it
    // failed instead of leaving a bar that reads as running on a dead task.
    let stage_now = std::cell::RefCell::new("clone");
    // One guard per running stage: dropping it (on the happy path or on a `?`)
    // stops the timer, so no stage keeps ticking after it is done or failed.
    let _clone_tick = StageTicker::start(task, "clone");
    task.log(&format!(
        "serving {ref_value} from the shared Harness mirror"
    ));
    let staged = layout.create_staging_dir("template_pull")?;
    let harness = staged.join("harness");
    let prepare = (|| -> Result<PreparedBaseRecord, String> {
        let mirror = read_config().ok().and_then(|config| config.github_mirror);
        let target = mirror_url(&parsed.url, mirror.as_deref());
        let revision = parsed.tag.as_deref().filter(|tag| *tag != "latest");
        // Every pull goes through the one shared mirror, so a version already
        // held costs no network at all and a new version costs only the objects
        // it does not share with the versions already there. The mirror lives
        // outside staging, so a failed pull keeps what it downloaded.
        let cancelled = {
            let flag = std::sync::Arc::clone(&cancel_flag);
            move || flag.lock().map(|guard| guard()).unwrap_or(true)
        };
        let commit = match stage_mirrored_checkout(task, &layout, &target, revision, &harness, cancelled) {
            Ok(commit) => commit,
            Err(error) => {
                // A transport failure means the network (or the mirror source) is
                // unreachable, not that the cache is damaged: say which, so the
                // user knows whether retrying later can work.
                let transport = error.transport;
                let message = error.into_message();
                // The SSL case keeps its configure-a-mirror hint: it is a
                // transport failure with a known fix, so the generic "(network)"
                // wording would be a step backwards from what users already get.
                return Err(if message.contains("SSL") {
                    format!(
                        "pull template failed: {message}\nhint: github.com may be blocked on this network; configure a mirror with `dshbox config set mirror <prefix>` (e.g. https://gh-proxy.com)"
                    )
                } else if transport {
                    format!("pull template failed (network; the shared mirror was kept): {message}")
                } else {
                    format!("pull template failed: {message}")
                });
            }
        };
        task.check_cancelled()?;

        configure_staged_pnpm_platform(&harness)?;
        let pnpm = resolve_toolchain("pnpm")?;
        let log_path = staged.join("prepare.log");
        drop(_clone_tick);
        task.stage_done("clone");
        *stage_now.borrow_mut() = "install";
        task.stage_started("install");
        let _install_tick = StageTicker::start(task, "install");
        task.log("installing DSH dependencies in prepared-base staging");
        run_pnpm_task(
            &pnpm,
            &harness,
            // Keep optional dependencies explicitly enabled. pnpm then selects
            // only the platform packages for the current host (for example
            // Windows x64 esbuild and koffi prebuilds), rather than downloading
            // every platform's optional binaries via `--force`.
            ["--config.optional=true", "install"],
            &log_path,
            task,
            "prepared-base dependency install",
        )?;
        task.check_cancelled()?;

        // Build the client artifacts here, once per Harness version, rather
        // than in every container: the build depends on the source, the commit
        // and the platform — not on the profile's plugins — so a prepared base
        // can carry it for every template built from it. This is the slow step
        // (native addon, host/client libraries, web frontend).
        drop(_install_tick);
        task.stage_done("install");
        *stage_now.borrow_mut() = "build";
        task.stage_started("build");
        let _build_tick = StageTicker::start(task, "build");
        task.log("building DSH client artifacts (native addon, host/client libs, web frontend); this is the slow step");
        let build_started = std::time::Instant::now();
        // `pnpm run build` is one child process, so its internal order is not
        // observable from here. Rather than invent sub-steps, the ticker above
        // advances the stage from elapsed time against its budget: the bar then
        // moves because time is really passing, and a stuck build freezes at 90%
        // with the elapsed time still climbing — which is the honest report.
        let build_result = run_pnpm_task_with(
            &pnpm,
            &harness,
            ["run", "build"],
            &log_path,
            task,
            "prepared-base client build",
            &[("DSH_CLIENT_COMMIT_HASH", commit.as_str())],
        );
        // Stop the ticker before reporting the outcome, so a finished stage
        // cannot be overwritten by a late tick.
        drop(_build_tick);
        if let Err(error) = build_result {
            // The failure is recorded once, by mark_stage_outcome on the way
            // out: marking it here as well would broadcast a failed stage
            // that the uniform path then overwrites.
            return Err(error);
        }
        task.log(&format!(
            "client artifacts built in {}s",
            build_started.elapsed().as_secs()
        ));
        crate::sealed::write_client_artifact_marker(&harness, &commit)?;
        task.check_cancelled()?;

        task.stage_done("build");
        *stage_now.borrow_mut() = "validate";
        task.stage_started("validate");
        let _validate_tick = StageTicker::start(task, "validate");
        validate_prepared_harness(&harness)?;
        drop(_validate_tick);
        let runtime = bundled_runtime()?;
        let created_at = now_seconds();
        let digest = box_dsh_versions::template_content_hash(&format!(
            "{}\n{}\n{}\n{}",
            ref_value, commit, runtime.node_version, runtime.pnpm_version
        ));
        let id = format!("base-{digest}");
        Ok(PreparedBaseRecord {
            id,
            version: parsed.version.clone(),
            source_ref: ref_value.clone(),
            source_commit: commit,
            directory: String::new(),
            created_at,
            size_bytes: 0,
        })
    })();

    let mut record = match prepare {
        Ok(record) => record,
        Err(error) => {
            mark_stage_outcome(task, &stage_now, &error, cancelled());
            // Staging is deliberately ephemeral, but the user-facing error
            // points at the pnpm transcript. Retain that transcript before
            // cleanup so a failed template pull is diagnosable after the task
            // completes.
            let diagnostic_dir = layout.root().join("logs").join("templates");
            let diagnostic_log = diagnostic_dir.join(format!("{}-pull.log", task.task_id));
            let _ = fs::create_dir_all(&diagnostic_dir);
            let retained = fs::copy(staged.join("prepare.log"), &diagnostic_log).is_ok();
            let _ = fs::remove_dir_all(&staged);
            return if retained {
                Err(format!(
                    "{error}; retained diagnostics at {}",
                    diagnostic_log.display()
                ))
            } else {
                Err(error)
            };
        }
    };
    // The prepared base is copied later as source only. Its staging-local pnpm
    // links are never followed or launched, so relinking them after publish
    // would only re-run the Windows junction failure we are avoiding.
    record.size_bytes = tree_size_bytes(&staged)?;
    let runtime = bundled_runtime()?;
    let manifest = TemplateManifest {
        schema_version: STORAGE_SCHEMA_VERSION,
        kind: PublishedResourceKind::PreparedBase,
        id: record.id.clone(),
        source_ref: record.source_ref.clone(),
        source_commit: record.source_commit.clone(),
        node_version: runtime.node_version.clone(),
        pnpm_version: runtime.pnpm_version.clone(),
        base_id: None,
        plugin_artifact_ids: Vec::new(),
        plugin_sources: Vec::new(),
        // A prepared base is a plain Harness tree: no Boxfile, so no
        // declared environment. A sealed template built on it fills this
        // in from its own `ENV` directives.
        env: BTreeMap::new(),
        harness_digest: record
            .id
            .strip_prefix("base-")
            .unwrap_or(&record.id)
            .to_owned(),
        profile_digest: None,
        size_bytes: record.size_bytes,
        validated_at: now_seconds(),
    };
    let publish_result = (|| -> Result<(), String> {
        write_template_manifest(&staged, &manifest)?;
        task.stage_done("validate");
        *stage_now.borrow_mut() = "publish";
        task.stage_started("publish");
        let _publish_tick = StageTicker::start(task, "publish");
        let destination =
            layout.prepared_base_dir(record.id.strip_prefix("base-").unwrap_or(&record.id))?;
        if destination.exists() {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!(
                "prepared base already exists at {}",
                destination.display()
            ));
        }
        layout.publish_staged_tree(&staged, &destination)?;
        record.directory = destination.to_string_lossy().into_owned();
        index.insert(record.version.clone(), record.clone());
        write_prepared_base_index(&layout, &index)?;
        let mut updated = read_config()?;
        updated.selected_dsh_version = Some(record.version.clone());
        write_config(&updated)?;
        task.stage_done("publish");
        Ok(())
    })();
    if let Err(error) = publish_result {
        mark_stage_outcome(task, &stage_now, &error, cancelled());
        return Err(error);
    }
    task.log(&format!(
        "prepared base {} ({}) is ready",
        record.version, record.id
    ));
    info!(
        "prepared Harness base {} (resolved version {})",
        ref_value, record.version
    );
    Ok(())
}

/// Serve one requested Harness version out of the shared mirror.
///
/// Three cases, in the order they are cheapest:
///
/// 1. the mirror already holds the commit (a second container of a version
///    installed before, or a retry after a failed build) - resolve it locally
///    and check out with no network call at all;
/// 2. the mirror is there but lacks the version - fetch just that ref;
/// 3. anything else - create the mirror, fetch, and check out.
///
/// A transport failure is not a reason to discard the mirror, so it is reported
/// with its own flag; a fetch that succeeded and still cannot resolve the ref
/// means the objects really are incomplete, and only then is the mirror rebuilt.
/// The mirror is named in the task log on the path that was taken: a user
/// wondering why a pull was fast or slow needs to see which one it was.
fn stage_mirrored_checkout(
    task: &TaskContext,
    layout: &RuntimeLayout,
    target: &str,
    revision: Option<&str>,
    harness: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<String, MirrorError> {
    // Both callbacks run inside libgit2's own transfer loop, so the task handle
    // has to be owned rather than borrowed from this frame.
    let progress = {
        let task = task.clone();
        move |percent: u8| task.stage_progress("clone", percent)
    };
    let callbacks = MirrorCallbacks::new(&cancelled).with_progress(&progress);
    let root = layout.root();
    let mirror = box_runtime::mirror::ensure_bare_mirror(root, target, &callbacks)?;
    // The cheap path first: a version this mirror already holds needs no network.
    if let Ok(commit) = box_runtime::mirror::resolve_in_mirror(&mirror, revision) {
        task.log(&format!(
            "shared mirror already holds {}; checking out without network access",
            revision.unwrap_or("the default branch")
        ));
        box_runtime::mirror::checkout_from_mirror(&mirror, harness, &commit, &callbacks)?;
        box_runtime::mirror::drop_checkout_metadata(harness);
        return Ok(commit);
    }
    let added = box_runtime::mirror::fetch_ref_to_mirror(&mirror, target, revision, &callbacks)?;
    task.log(&format!(
        "fetched {} through the shared mirror ({} new objects; mirror is {} MB)",
        revision.unwrap_or("the default branch"),
        added,
        box_runtime::mirror::mirror_size_bytes(&mirror) / (1024 * 1024)
    ));
    let commit = match box_runtime::mirror::resolve_in_mirror(&mirror, revision) {
        Ok(commit) => commit,
        Err(missing) => {
            // Do NOT rebuild yet. A fetch of an explicit refspec for a tag the
            // remote does not have returns Ok with zero objects (measured), so
            // "the fetch succeeded but the ref is missing" is also exactly what
            // a mistyped version number or a deleted upstream tag looks like.
            // Deleting the mirror there would throw away every version it holds
            // because somebody typed a version wrong, so the remote is asked
            // whether the ref exists before any repair happens.
            let advertised = box_runtime::mirror::remote_has_revision(&mirror, target, revision)?;
            if !advertised {
                return Err(MirrorError {
                    transport: false,
                    message: format!(
                        "{} is not a version this remote advertises ({}); the shared mirror was kept as-is",
                        revision.unwrap_or("the default branch"),
                        target,
                    ),
                });
            }
            // The remote does advertise it and the mirror still cannot resolve
            // it, so the objects really are incomplete. Rebuild once - the only
            // repair the design allows - and say so in the log.
            task.log("shared mirror is missing objects for a version the remote advertises; rebuilding it once");
            let mirror = box_runtime::mirror::rebuild_bare_mirror(root, target, &callbacks)?;
            box_runtime::mirror::fetch_ref_to_mirror(&mirror, target, revision, &callbacks)?;
            box_runtime::mirror::resolve_in_mirror(&mirror, revision).map_err(|error| {
                MirrorError {
                    transport: false,
                    message: format!("{}; rebuilding did not fix it: {}", missing.message, error.message),
                }
            })?
        }
    };
    box_runtime::mirror::checkout_from_mirror(&mirror, harness, &commit, &callbacks)?;
    box_runtime::mirror::drop_checkout_metadata(harness);
    Ok(commit)
}

/// Mark the stage that was running when the pull failed, so a dead task never
/// leaves a stage bar reading as still running. A cancellation is not a
/// failure: the stage is skipped, anything else records the failure detail.
fn mark_stage_outcome(
    task: &TaskContext,
    stage_now: &std::cell::RefCell<&'static str>,
    error: &str,
    cancelled: bool,
) {
    let stage = *stage_now.borrow();
    // Ask the cancel flag rather than reading the error text. The message is
    // spelled in several places across crates ("task cancelled", "task
    // cancelled while {description}"), and a cancelled clone surfaces as a
    // libgit2 error ("Git clone failed: ...") with the cancellation nowhere
    // in it — matching on text would call that a failure.
    if cancelled || error.starts_with("task cancelled") {
        task.stage_skipped(stage);
    } else {
        task.stage_failed(stage, error);
    }
}

/// pnpm's lockfile resolver needs an explicit supported-architectures entry
/// when materialising DSH's native optional dependency graph from an empty
/// store. Without it, pnpm recognises the current platform but can leave the
/// matching esbuild, koffi, and lefthook prebuilds unlinked. This only updates
/// the private staging clone, never the user's source repository.
fn configure_staged_pnpm_platform(harness: &Path) -> Result<(), String> {
    let workspace = harness.join("pnpm-workspace.yaml");
    let contents = fs::read_to_string(&workspace)
        .map_err(|error| format!("cannot read {}: {error}", workspace.display()))?;
    if contents
        .lines()
        .any(|line| line.trim() == "supportedArchitectures:")
    {
        return Ok(());
    }
    let os = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let cpu = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        architecture => architecture,
    };
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&workspace)
        .map_err(|error| format!("cannot update {}: {error}", workspace.display()))?;
    writeln!(
        file,
        "\nsupportedArchitectures:\n  os:\n    - {os}\n  cpu:\n    - {cpu}"
    )
    .map_err(|error| format!("cannot write {}: {error}", workspace.display()))
}

/// A stage that ticks itself while it runs.
///
/// Drives `stage_tick` on a timer so a stage's bar moves because time really
/// is passing, not because a number was written once. Dropping it stops the
/// timer, which is why every stage below holds one in a local binding until
/// it is done: the guard is the lifetime, so a `?` on an error path stops the
/// ticking too instead of leaving a thread behind.
struct StageTicker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StageTicker {
    fn start(task: &TaskContext, stage_id: &str) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = stop.clone();
        let context = task.clone();
        let id = stage_id.to_owned();
        let handle = std::thread::spawn(move || {
            while flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                context.stage_tick(&id);
            }
        });
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for StageTicker {
    fn drop(&mut self) {
        self.stop.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
fn run_pnpm_task<const N: usize>(
    pnpm: &crate::toolchains::ResolvedToolchain,
    directory: &Path,
    args: [&str; N],
    log_path: &Path,
    task: &TaskContext,
    label: &str,
) -> Result<(), String> {
    run_pnpm_task_with(pnpm, directory, args, log_path, task, label, &[])
}

/// `run_pnpm_task` with extra environment for the child: the client build needs
/// the same commit hash the container build would embed, or the artifacts it
/// produces are not the ones a container would have built.
fn run_pnpm_task_with<const N: usize>(
    pnpm: &crate::toolchains::ResolvedToolchain,
    directory: &Path,
    args: [&str; N],
    log_path: &Path,
    task: &TaskContext,
    label: &str,
    overrides: &[(&str, &str)],
) -> Result<(), String> {
    let directory_arg = directory.to_string_lossy().into_owned();
    let mut policy = pnpm_policy(pnpm)?;
    for (name, value) in overrides {
        policy = policy.task_override(*name, *value);
    }
    let spec = ProcessSpec::new(&pnpm.path)
        .args(&pnpm.arguments)
        .args(["--dir", directory_arg.as_str()])
        .args(args)
        .cwd(directory)
        .policy(policy)
        .kind(ExecutionKind::Logged)
        .log_path(log_path);
    let mut process = run_logged(&spec, label)?;
    let status = process
        .wait_or_kill(&TaskCancel(Some(task)), Duration::from_secs(900), label)
        .map_err(|error| format!("{label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        crate::sealed::log_command_tail(log_path, task, 20);
        let hint = pnpm_network_failure_hint(log_path).unwrap_or_default();
        Err(format!(
            "{label} failed; inspect {}.{hint}",
            log_path.display()
        ))
    }
}

fn validate_prepared_harness(harness: &Path) -> Result<(), String> {
    for relative in [
        "package.json",
        "node_modules/tsx/package.json",
        "apps/cli/src/bin.ts",
        // The host launches this built entry, so a base without it cannot
        // start a container at all — see `dshboxd::lifecycle` for why the
        // source entry is not an acceptable substitute.
        "apps/cli/lib/bin.js",
        // The client build runs while the base is prepared, so a base without
        // its artifacts is incomplete: containers would silently fall back to
        // building them one by one.
        "apps/web/dist/index.html",
        crate::sealed::CLIENT_ARTIFACT_MARKER,
    ] {
        if !harness.join(relative).is_file() {
            return Err(format!(
                "prepared Harness validation failed: missing {relative}"
            ));
        }
    }
    // The Windows-native optional packages are what postinstall scripts
    // (esbuild, koffi) and the frontend build (lefthook) resolve at runtime.
    // A store that lost them — interrupted installs, antivirus quarantine of
    // the prebuilt binaries — otherwise surfaces much later as confusing
    // "Failed to find package @esbuild/win32-x64" / CMake-not-found errors.
    if !cfg!(target_os = "windows") {
        return Ok(());
    }
    let optional_prefixes = ["@esbuild+win32-x64@", "@koromix+koffi-win32-x64@"];
    let mut found = [false; 2];
    if let Ok(entries) = harness.join("node_modules/.pnpm").read_dir() {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            for (index, prefix) in optional_prefixes.iter().enumerate() {
                if name.starts_with(prefix) && entry.path().join("node_modules").is_dir() {
                    found[index] = true;
                }
            }
        }
    }
    for (index, prefix) in optional_prefixes.iter().enumerate() {
        if !found[index] {
            return Err(format!(
                "prepared Harness validation failed: the Windows package `{}` was not \
                 materialized. The pnpm store may have been damaged by an interrupted \
                 install or antivirus quarantine; retry the pull, and if it persists add \
                 the DSH Box data directory to your antivirus exclusions.",
                prefix.trim_end_matches('@')
            ));
        }
    }
    Ok(())
}

fn tree_size_bytes(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            total = total.saturating_add(tree_size_bytes(&entry.path())?);
        } else if file_type.is_file() {
            total =
                total.saturating_add(entry.metadata().map_err(|error| error.to_string())?.len());
        }
    }
    Ok(total)
}

/// Uninstall a DSH harness: soft-delete the runtime clone, drop the
/// template index entry, and enqueue the background hard-delete via the
/// `box_state` deletion queue.
///
/// Delegates to `box_template_core::uninstall_template` so the daemon and
/// the CLI share the same soft-delete logic (rename + resource-map cleanup
/// + deletion-queue enqueue). The actual `remove_dir_all` happens in the
/// background deletion worker, not in this call — the function returns
/// within a few hundred milliseconds.
pub(crate) fn uninstall_dsh_version(version: &str) -> Result<(), String> {
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let mut index = read_prepared_base_index(&layout)?;
    let record = index
        .remove(version)
        .ok_or_else(|| format!("prepared Harness base `{version}` is not installed"))?;
    let directory = PathBuf::from(&record.directory);
    let templates = layout.templates_dir();
    if !directory.starts_with(&templates)
        || !directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with("base-"))
            .unwrap_or(false)
    {
        return Err("prepared-base index contains an unsafe directory".to_owned());
    }
    if directory.exists() {
        fs::remove_dir_all(&directory).map_err(|error| {
            format!(
                "cannot remove prepared base {}: {error}",
                directory.display()
            )
        })?;
    }
    write_prepared_base_index(&layout, &index)?;
    // Also clear the selected version if it pointed at this tag.
    let mut config = read_config()?;
    if config.selected_dsh_version.as_deref() == Some(version) {
        config.selected_dsh_version = None;
        let _ = write_config(&config);
    }
    Ok(())
}




#[cfg(test)]
#[path = "versions_tests.rs"]
mod tests;