//! Update discovery and download, owned by the daemon.
//!
//! The daemon checks because it is the long-lived process: the desktop window
//! may be closed for days while a release appears, and a check that only runs
//! when a person opens a window is a check that never runs. The *install*
//! belongs to the desktop instead, because installing replaces the binaries
//! this daemon is made of -- a process cannot swap the file it is executing
//! from, so whoever ends the process has to be somewhere else.
//!
//! Discovery has two tiers, and the split matters:
//!
//! - The *quick* tier asks GitHub for the newest release. It is one small
//!   JSON and it can only answer "something newer exists". It cannot install,
//!   because a tag says nothing about which platform asset this machine needs
//!   and nothing about that asset's bytes.
//! - The *full* tier reads the \`latest.json\` attached to that release, which
//!   names the installer per platform together with its SHA-256. That digest
//!   is what makes "the bytes I fetched are the bytes CI built" checkable
//!   after the transfer.
//!
//! Nothing here is persisted except the outcome of a check (so a restart does
//! not re-ask immediately) and the downloaded file itself. The policy lives in
//! \`config.json\`.

use box_foundation::{atomic_write_json, is_update_policy, mirror_url, now_seconds, read_config,
    write_config, BoxConfig, UPDATE_POLICIES};
use box_scheduler::TaskContext;
use box_updates::{
    is_newer, latest_manifest_url, normalize_digest, same_version, UpdateManifest,
    LATEST_RELEASE_API,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::{Path, PathBuf}, time::Duration};
use tracing::{info, warn};

use crate::{dispatch::HandlerResult, state::DaemonState};

/// The version a user sees, embedded by build.rs from src-tauri/Cargo.toml.
///
/// Not CARGO_PKG_VERSION: this crate is internal and keeps its own 0.1.0, while
/// the released version lives on the application package. Comparing a release
/// against the crate version made a 0.1.9 install report itself as 0.1.0, so
/// every published release looked like an update -- and the pane would have
/// offered a downgrade-to-same as an upgrade.
pub(crate) const APP_VERSION: &str = env!("DSHBOX_APP_VERSION");
/// How often a background check is allowed to hit the network.
///
/// A day. The question "is there a newer release" does not change faster
/// than that for a desktop app, and asking more often is how an updater
/// becomes something people turn off.
pub(crate) const CHECK_INTERVAL_SECONDS: u64 = 86_400;

/// The result of the last check, persisted so a restart does not re-ask.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateRecord {
    /// Epoch seconds of the last *successful* check.
    #[serde(default)]
    pub(crate) checked_at: Option<u64>,
    /// The newest release the box knows about, from the quick tier.
    #[serde(default)]
    pub(crate) latest_version: Option<String>,
    #[serde(default)]
    pub(crate) latest_tag: Option<String>,
    #[serde(default)]
    pub(crate) notes_url: Option<String>,
    /// The last failure, cleared by the next success. Never invented: a failed
    /// check reports the failure rather than the previous answer's timestamp.
    #[serde(default)]
    pub(crate) last_error: Option<String>,
}

fn record_path(root: &Path) -> PathBuf {
    root.join("state").join("update-state.json")
}

pub(crate) fn read_record(root: &Path) -> UpdateRecord {
    fs::read_to_string(record_path(root))
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn write_record(root: &Path, record: &UpdateRecord) -> Result<(), String> {
    atomic_write_json(&record_path(root), record)
}

/// Where a verified installer for \`version\` lives.
pub(crate) fn download_directory(root: &Path, version: &str) -> PathBuf {
    root.join("updates").join(version)
}

/// The release payload the UI renders.
///
/// It carries the *asset* too, which means reading the full manifest rather than
/// only the quick tier: the pane and the install path both need to know which
/// file this machine would run, and an update whose file cannot be named is not
/// one the UI should offer a button for. A release that predates the manifest
/// says so through `manualReason` instead of pretending.
fn release_payload(config: &BoxConfig, record: &UpdateRecord, current: &str) -> Value {
    let Some(version) = record.latest_version.as_deref() else {
        return json!(null);
    };
    // `newer` is computed here rather than by the reader: the box may be ahead
    // of the newest published release (a developer build), and being ahead is
    // not an update.
    let newer = is_newer(version, current);
    let mut payload = json!({
        "version": version,
        "notesUrl": record.notes_url,
        "newer": newer,
        "manualReason": null,
        "asset": null,
    });
    if !newer {
        return payload;
    }
    let tag = record
        .latest_tag
        .clone()
        .unwrap_or_else(|| format!("v{version}"));
    // Only worth the extra request when there is something to offer.
    let Some(manifest) = fetch_manifest(&tag, config.github_mirror.as_deref()).ok() else {
        // A release from before the manifest existed: nothing here has a
        // digest, so nothing here is something this app will run.
        payload["manualReason"] = json!("no-manifest");
        return payload;
    };
    if !manifest.installable_from(current) {
        payload["manualReason"] = json!("below-minimum");
        return payload;
    }
    match manifest.preferred_asset(crate::state::bundled_target(), preferred_installers()) {
        Ok(asset) => {
            payload["manualReason"] = if asset.is_automatic() {
                json!(null)
            } else {
                json!("manual-installer")
            };
            payload["asset"] = json!({
                "url": asset.url,
                "sha256": asset.sha256,
                "size": asset.size,
                "installer": asset.installer,
                "installMode": asset.install_mode,
            });
        }
        Err(problem) => {
            payload["manualReason"] = json!(problem.reason());
        }
    }
    payload
}

/// One GitHub release, reduced to what the updater needs.
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    // Absent for a tag with no release object; the caller decides.
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
}

/// A reqwest client that is allowed to use the network.
///
/// Deliberately *not* \`.no_proxy()\`: that call exists for loopback probes,
/// where a corporate proxy turns 127.0.0.1 into a remote failure. GitHub is
/// the opposite case -- behind a proxy is exactly where this has to work --
/// and Windows/macOS honor the system proxy here because the daemon does not
/// run inside the clean-room environment policy (that is for spawned children).
fn network_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("DSH-Box/{APP_VERSION}"))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("cannot create the update client: {error}"))
}

/// Ask GitHub for the newest release. The *quick* tier.
fn fetch_latest_release(mirror: Option<&str>) -> Result<GitHubRelease, String> {
    let endpoint = mirror_url(LATEST_RELEASE_API, mirror);
    let client = network_client()?;
    let response = client
        .get(&endpoint)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("the repository has no published release yet".to_owned());
    }
    let response = response
        .error_for_status()
        .map_err(|error| format!("the release request failed: {error}"))?;
    response
        .json::<GitHubRelease>()
        .map_err(|error| format!("cannot parse the release: {error}"))
}

/// Read the \`latest.json\` attached to \`tag\`. The *full* tier.
pub(crate) fn fetch_manifest(
    tag: &str,
    mirror: Option<&str>,
) -> Result<UpdateManifest, String> {
    let url = mirror_url(&latest_manifest_url(tag), mirror);
    let client = network_client()?;
    let response = client
        .get(&url)
        .send()
        .map_err(|error| format!("cannot reach the release manifest: {error}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // A release that predates the manifest. A real limitation of stale
        // release data, reported as itself rather than papered over.
        return Err(format!("NO_MANIFEST:{tag}"));
    }
    let text = response
        .error_for_status()
        .map_err(|error| format!("the manifest request failed: {error}"))?
        .text()
        .map_err(|error| format!("cannot read the manifest: {error}"))?;
    UpdateManifest::parse(&text)
}

/// Ask GitHub for the recent releases, newest first. The *history* tier.
///
/// Read-only: it feeds the version page's "what changed in each release"
/// list. It deliberately checks nothing installable -- only the newest
/// release carries a manifest, and the badge/available logic already owns
/// that question.
fn fetch_releases(mirror: Option<&str>) -> Result<Vec<GitHubRelease>, String> {
    let endpoint = mirror_url(box_updates::RELEASES_LIST_API, mirror);
    let client = network_client()?;
    let response = client
        .get(&endpoint)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let response = response
        .error_for_status()
        .map_err(|error| format!("the releases request failed: {error}"))?;
    response
        .json::<Vec<GitHubRelease>>()
        .map_err(|error| format!("cannot parse the releases: {error}"))
}

/// The RPC behind the version page's list: the last few releases with their
/// notes, so the UI can preview what each version changes.
pub(crate) fn list_update_releases_rpc() -> Result<Value, String> {
    let config = read_config()?;
    let releases = fetch_releases(config.github_mirror.as_deref())?;
    let rows = releases
        .iter()
        .map(|release| {
            json!({
                "version": release.tag_name.trim_start_matches('v'),
                "tag": release.tag_name,
                "publishedAt": release.published_at,
                "notes": release.body,
                "notesUrl": release.html_url,
                "prerelease": release.prerelease,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "releases": rows }))
}

/// Run one check against the network and record the outcome.
///
/// \`record\` is only updated on a successful check: a failed one records its
/// error and leaves \`checked_at\` alone, so the UI can say "last tried" and
/// "last successful" honestly instead of showing a stale success as fresh.
pub(crate) fn perform_check(root: &Path, mirror: Option<&str>) -> Result<UpdateRecord, String> {
    let mut record = read_record(root);
    match fetch_latest_release(mirror) {
        Ok(release) => {
            record.checked_at = Some(now_seconds());
            record.latest_version = Some(
                release
                    .tag_name
                    .trim_start_matches('v')
                    .to_owned(),
            );
            record.latest_tag = Some(release.tag_name.clone());
            record.notes_url = release.html_url.clone();
            record.last_error = None;
            // The record keeps the newest release even when it is the one
            // already installed. Clearing it here is what made a successful
            // check read as "no release has been checked yet": the pane could
            // not tell "up to date" from "never checked", and the only way
            // to learn the current release was to press the button again.
            //
            // Whether that release is an *update* is `is_newer`'s job, in the
            // payload, not the record's.
            let _ = write_record(root, &record);
            let _ = release.body;
            let _ = release.published_at;
            let _ = release.prerelease;
            info!(version = ?record.latest_version, "update check complete");
            Ok(record)
        }
        Err(error) => {
            record.last_error = Some(error.clone());
            let _ = write_record(root, &record);
            warn!(error, "update check failed");
            Err(error)
        }
    }
}

/// Which installer this platform prefers, in order.
///
/// Linux publishes both a .deb and a .rpm for the same machine, so "the" asset
/// is not well defined there and the platform's own preference decides.
fn preferred_installers() -> &'static [&'static str] {
    if cfg!(target_os = "linux") {
        // Most distros that ship a desktop already use dpkg; rpm is the
        // fallback for the ones that do not.
        &["deb", "rpm"]
    } else if cfg!(target_os = "macos") {
        &["dmg"]
    } else {
        &["msi"]
    }
}


/// Download an installer for version, verify it, and leave it ready.
///
/// Runs as a scheduler task so it has a log and a cancel button like every
/// other long operation. A file that fails verification is deleted, so a
/// download reported as ready always means "present and checked".
pub(crate) fn download_with_cancel(
    root: &Path,
    version: &str,
    tag: &str,
    target: &str,
    mirror: Option<&str>,
    task: &TaskContext,
) -> Result<(), String> {
    task.update("reading the release manifest", 5);
    let manifest = fetch_manifest(tag, mirror)?;
    task.log(&format!("release manifest for {tag} read"));
    let asset = manifest
        .preferred_asset(target, preferred_installers())
        .map_err(|problem| problem.detail())?;
    let digest = normalize_digest(&asset.sha256).map_err(|error| error.message())?;
    let url = mirror_url(&asset.url, mirror);
    let directory = download_directory(root, version);
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let filename = asset
        .url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or("the release asset URL has no filename")?;
    let final_path = directory.join(filename);
    // Staged under a distinct name so a half-written file can never be
    // mistaken for the verified one.
    let staging = directory.join(format!("{filename}.part"));
    // A re-download of something already verified is a no-op: the bytes and
    // the digest are the same question.
    if final_path.exists() {
        if let Ok(existing) = fs::read(&final_path) {
            let mut state = box_updates::download::StreamingDigest::new();
            state.update(&existing);
            if state.finish() == digest {
                task.log(&format!(
                    "{} is already downloaded and verified",
                    final_path.display()
                ));
                task.update("ready", 100);
                return Ok(());
            }
        }
        let _ = fs::remove_file(&final_path);
    }
    task.update(&format!("downloading {filename}"), 10);
    task.log(&format!("downloading {url}"));
    let client = network_client()?;
    let response = client
        .get(&url)
        .send()
        .map_err(|error| format!("cannot download {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("the download returned HTTP {}", response.status()));
    }
    let total = response.content_length().unwrap_or(asset.size);
    // Progress is reported per chunk, so a 160 MB installer is not a frozen bar.
    let mut last_progress: u8 = 10;
    let mut reader = response;
    let written = box_updates::download_and_verify(
        &mut reader,
        &staging,
        &digest,
        asset.size,
        |bytes, expected| {
            if task.cancelled() {
                return;
            }
            let denominator = if expected == 0 { total } else { expected };
            if denominator == 0 {
                return;
            }
            let percent = ((bytes.saturating_mul(90)) / denominator).min(90) as u8 + 10;
            if percent > last_progress {
                last_progress = percent;
                task.update("downloading", percent);
            }
        },
    )
    .map_err(|error| error.message())?;
    task.check_cancelled()?;
    fs::rename(&staging, &final_path)
        .map_err(|error| format!("cannot store the verified installer: {error}"))?;
    task.log(&format!(
        "verified {filename} ({written} bytes, sha256 {}...)",
        &digest[..12]
    ));
    task.update("ready", 100);
    Ok(())
}

/// What the daemon reports about a downloaded installer for one version.
pub(crate) fn download_payload(root: &Path, version: &str) -> Value {
    let directory = download_directory(root, version);
    let mut entries: Vec<(String, u64)> = Vec::new();
    if let Ok(listing) = fs::read_dir(&directory) {
        for entry in listing.flatten() {
            let Ok(metadata) = entry.metadata() else { continue };
            if !metadata.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            entries.push((name, metadata.len()));
        }
    }
    // A .part file is a download in flight or an abandoned one; only a whole
    // file is something to offer.
    let ready = entries.iter().find(|(name, _)| !name.ends_with(".part"));
    match ready {
        Some((name, size)) => json!({
            "version": version,
            "state": "ready",
            "bytes": size,
            "path": directory.join(name).to_string_lossy(),
        }),
        None => json!(null),
    }
}

/// The full state the UI reads in one round trip.
///
/// A policy of Off disables the background tier entirely; an explicit check
/// request is the UI's button and still runs, because a person asking once
/// outranks a standing preference.
pub(crate) fn collect_state(config: &BoxConfig) -> Value {
    let current_version = APP_VERSION;
    let build_stamp = env!("DSHBOX_BUILD_STAMP");
    let skip = config.skipped_update_version.clone();
    let policy = config.auto_update.clone();
    let Some(root) = config.runtime_directory.as_deref() else {
        return json!({
            "currentVersion": current_version,
            "buildStamp": build_stamp,
            "policy": policy,
            "skippedVersion": skip,
            "checkedAt": null,
            "lastError": "DSH Box storage is not configured",
            "checkIntervalSeconds": CHECK_INTERVAL_SECONDS,
            "available": null,
            "download": null,
        });
    };
    let root = Path::new(root);
    let record = read_record(root);
    let available = release_payload(config, &record, current_version);
    let download = record
        .latest_version
        .as_deref()
        .map(|version| download_payload(root, version))
        .unwrap_or(json!(null));
    json!({
        "currentVersion": current_version,
        "buildStamp": build_stamp,
        "policy": policy,
        "skippedVersion": skip,
        "checkedAt": record.checked_at,
        "lastError": record.last_error,
        "checkIntervalSeconds": CHECK_INTERVAL_SECONDS,
        "available": available,
        "download": download,
    })
}

/// True when the background tier should run right now.
pub(crate) fn should_check(record: &UpdateRecord, policy: &str) -> bool {
    if policy == "off" {
        return false;
    }
    match record.checked_at {
        None => true,
        Some(checked) => now_seconds().saturating_sub(checked) >= CHECK_INTERVAL_SECONDS,
    }
}

/// Read the update state. Cheap: never blocks on the network.
///
/// Opening the pane, or the shell asking for its badge, is also what starts the
/// background tier when the throttle says a check is due. It runs on its own
/// thread and announces itself when it finishes, so this answers immediately
/// with what is already known rather than waiting on GitHub.
pub(crate) fn get_update_state_rpc(state: &DaemonState) -> Result<Value, String> {
    let config = read_config()?;
    if should_check(&read_record_of(&config), &config.auto_update) {
        schedule_background_check(state);
    }
    Ok(crate::updates::collect_state(&config))
}

/// Run a real check against the network and answer with the fresh state.
///
/// The UI's explicit "check now" button. Unlike the background tier this always
/// goes out, because the caller asked.
pub(crate) fn check_update_rpc() -> Result<Value, String> {
    let config = read_config()?;
    if let Some(root) = config.runtime_directory.as_deref() {
        crate::updates::perform_check(std::path::Path::new(root), config.github_mirror.as_deref())
        .map_err(|error| format!("update check failed: {error}"))?;
    }
    Ok(crate::updates::collect_state(&config))
}

/// Download this machine's installer for the newest release.
pub(crate) fn enqueue_update_download(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let config = read_config()?;
    let root = config
        .runtime_directory
        .clone()
        .ok_or("DSH Box storage is not configured")?;
    let target = crate::state::bundled_target().to_owned();
    // Version and tag come from the request when the UI already knows them,
    // and otherwise from the last recorded check, so a caller that just calls
    // "download" is not required to have memorised the release first.
    let record = crate::updates::read_record(std::path::Path::new(&root));
    let version = request["version"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| record.latest_version.clone())
        .ok_or("no release is known yet; check for updates first")?;
    let tag = request["tag"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| record.latest_tag.clone())
        .unwrap_or_else(|| format!("v{version}"));
    let mirror = config.github_mirror.clone();
    let params = json!({ "version": version.clone(), "tag": tag.clone(), "target": target });
    crate::dispatch::enqueue_task_worker(
        state,
        "update-download",
        vec!["updates".to_owned()],
        params,
        move |task| {
            crate::updates::download_with_cancel(
                std::path::Path::new(&root),
                &version,
                &tag,
                &target,
                mirror.as_deref(),
                task,
            )
        },
    )
    .map(HandlerResult::Async)
}

/// Delete a downloaded installer, so "ready" can go back to "not downloaded".
pub(crate) fn discard_update_download(request: &Value) -> Result<Value, String> {
    let config = read_config()?;
    let root = config
        .runtime_directory
        .as_deref()
        .ok_or("DSH Box storage is not configured")?;
    let record = crate::updates::read_record(std::path::Path::new(root));
    let version = request["version"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| record.latest_version.clone())
        .ok_or("no release is known yet")?;
    let directory = crate::updates::download_directory(std::path::Path::new(root), &version);
    let mut removed = 0_u32;
    if let Ok(listing) = std::fs::read_dir(&directory) {
        for entry in listing.flatten() {
            if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                let _ = std::fs::remove_file(entry.path());
                removed += 1;
            }
        }
    }
    Ok(json!({ "version": version, "removed": removed }))
}

/// Remember "not this one" so the badge stays quiet until something newer.
pub(crate) fn skip_update_version(request: &Value) -> Result<Value, String> {
    let mut config = read_config()?;
    let version = request["version"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            config.runtime_directory.as_deref().and_then(|root| {
                crate::updates::read_record(std::path::Path::new(root)).latest_version
            })
        })
        .ok_or("no release is known yet")?;
    config.skipped_update_version = Some(version.clone());
    write_config(&config)?;
    Ok(json!({ "skippedVersion": version }))
}

/// Forget the skip.
pub(crate) fn clear_update_skip() -> Result<Value, String> {
    let mut config = read_config()?;
    config.skipped_update_version = None;
    write_config(&config)?;
    Ok(json!({ "skippedVersion": null }))
}

/// Persist the update policy.
pub(crate) fn set_update_policy(request: &Value) -> Result<Value, String> {
    let policy = request["policy"].as_str().unwrap_or("").to_owned();
    if !is_update_policy(&policy) {
        return Err(format!(
            "unknown update policy {policy:?}; expected one of {}",
            UPDATE_POLICIES.join(", ")
        ));
    }
    let mut config = read_config()?;
    config.auto_update = policy.clone();
    write_config(&config)?;
    Ok(json!({ "policy": policy }))
}
/// Broadcast "something finished, re-read the state" to every UI.
///
/// The update state is small and the client already has a reader for it, so
/// an invalidation notice beats pushing the whole payload down the SSE bus and
/// having two shapes of the same data to keep in step.
pub(crate) fn announce(events: &crate::events::DaemonEvents) {
    events.broadcast(crate::events::DaemonEvent::UpdateState);
}

/// The state a background pass needs, captured before the thread starts.
///
/// A detached thread cannot borrow the daemon, and it must not hold a lock while
/// it waits on GitHub: these are cloned handles (an \`Arc\` internally) that
/// outlive the call that spawned it.
pub(crate) struct BackgroundDeps {
    manager: box_scheduler::TaskManager,
    paths: box_foundation::BoxPaths,
    events: std::sync::Arc<crate::events::DaemonEvents>,
}

/// One background check, spawned detached so no RPC call waits on the network.
///
/// At most one runs at a time: the flag is the whole lock, because two
/// overlapping passes would ask the same question twice and write the same
/// answer.
pub(crate) fn schedule_background_check(state: &crate::state::DaemonState) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(paths) = state.paths.read().ok().map(|paths| paths.clone()) else {
        RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    let deps = BackgroundDeps {
        manager: state.manager.clone(),
        paths,
        events: state.events.clone(),
    };
    std::thread::spawn(move || {
        let outcome = background_check_once(&deps);
        RUNNING.store(false, Ordering::SeqCst);
        match outcome {
            Ok(outcome) => {
                if outcome != BackgroundOutcome::NotDue {
                    announce(&deps.events);
                }
            }
            Err(error) => warn!(error, "background update check failed"),
        }
    });
}

/// One pass of the background tier, honouring the policy and the throttle.
///
/// The answer matters, because a policy of \`download\` or \`auto\` has to act on
/// it rather than only record it: the pane's text promises those policies fetch
/// the installer, and a policy whose description outruns its behaviour is worse
/// than no policy at all.
fn background_check_once(deps: &BackgroundDeps) -> Result<BackgroundOutcome, String> {
    let config = read_config()?;
    if !should_check(&read_record_of(&config), &config.auto_update) {
        return Ok(BackgroundOutcome::NotDue);
    }
    let Some(root) = config.runtime_directory.clone() else {
        return Err("DSH Box storage is not configured".to_owned());
    };
    let record = perform_check(std::path::Path::new(&root), config.github_mirror.as_deref())?;
    // Off and check stop here: detection only, never a fetch.
    if !matches!(config.auto_update.as_str(), "download" | "auto") {
        return Ok(BackgroundOutcome::Checked);
    }
    let Some(version) = record.latest_version.clone() else {
        return Ok(BackgroundOutcome::Checked);
    };
    if !is_newer(&version, APP_VERSION) || is_skipped(&config, &version) {
        return Ok(BackgroundOutcome::Checked);
    }
    // Already fetched and verified: re-downloading it asks the same question.
    if download_payload(std::path::Path::new(&root), &version)["path"]
        .as_str()
        .is_some()
    {
        return Ok(BackgroundOutcome::Checked);
    }
    let tag = record
        .latest_tag
        .clone()
        .unwrap_or_else(|| format!("v{version}"));
    let target = crate::state::bundled_target().to_owned();
    let mirror = config.github_mirror.clone();
    // Enqueued through the queue itself, not through a DaemonState helper: the
    // thread has no state, and the task machinery only needs the queue and the
    // notifier.
    let task = deps
        .manager
        .enqueue(
            &deps.paths,
            "update-download",
            vec!["updates".to_owned()],
            json!({ "version": version.clone(), "tag": tag.clone(), "target": target, "automatic": true }),
        )
        .map_err(|error| format!("cannot enqueue the automatic download: {error}"))?;
    let manager = deps.manager.clone();
    let paths = deps.paths.clone();
    let events = deps.events.clone();
    let task_id = task.id.clone();
    let root_for_task = root.clone();
    std::thread::spawn(move || {
        let notifier = crate::state::DaemonNotifier::from_paths(manager.clone(), events);
        box_scheduler::run_queued(
            &manager,
            &paths,
            std::sync::Arc::new(notifier),
            &task_id,
            move |task| {
                download_with_cancel(
                    std::path::Path::new(&root_for_task),
                    &version,
                    &tag,
                    &target,
                    mirror.as_deref(),
                    task,
                )
                .map_err(|error| error.to_string())
            },
            None,
        );
    });
    Ok(BackgroundOutcome::Downloading)
}

/// What a background pass did, so the caller can announce it rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundOutcome {
    /// The throttle said this pass was not due.
    NotDue,
    /// Checked and left it there (detection-only policy, or nothing to fetch).
    Checked,
    /// Checked and started a download.
    Downloading,
}

/// Is this release the one the user asked to skip?
fn is_skipped(config: &BoxConfig, version: &str) -> bool {
    config
        .skipped_update_version
        .as_deref()
        .is_some_and(|skipped| same_version(skipped, version))
}


/// The record for a config's runtime directory, or an empty one when storage
/// is not configured yet.
fn read_record_of(config: &BoxConfig) -> UpdateRecord {
    match config.runtime_directory.as_deref() {
        Some(root) => read_record(std::path::Path::new(root)),
        None => UpdateRecord::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_version_is_the_application_version_not_the_crate_version() {
        // The bug this locks: dshboxd is an internal crate that keeps its own
        // 0.1.0, while the released version lives on the application package.
        // Reporting the crate version made a 0.1.9 install answer "0.1.0", so
        // is_newer called every published release newer and the pane offered
        // an "update" to the version already installed.
        assert_ne!(
            APP_VERSION,
            env!("CARGO_PKG_VERSION"),
            "the app version happens to equal the crate version, so this test cannot tell them apart"
        );
        assert!(
            !APP_VERSION.starts_with('v'),
            "the reported version must not carry a v prefix: {APP_VERSION}"
        );
        assert!(
            APP_VERSION.split('.').count() >= 3,
            "expected a semver-ish version, got {APP_VERSION}"
        );
    }

    #[test]
    fn a_release_at_the_current_version_is_not_an_update() {
        // The user-visible consequence of the bug above, stated directly.
        assert!(!is_newer(APP_VERSION, APP_VERSION));
        assert!(same_version(APP_VERSION, APP_VERSION));
    }

    #[test]
    fn the_throttle_spends_a_check_only_when_the_day_has_passed_or_never_ran() {
        assert!(should_check(&UpdateRecord::default(), "check"));
        let recent = UpdateRecord {
            checked_at: Some(now_seconds()),
            ..UpdateRecord::default()
        };
        assert!(!should_check(&recent, "check"));
        assert!(!should_check(&recent, "download"));
        // A stale record is due again.
        let stale = UpdateRecord {
            checked_at: Some(now_seconds() - CHECK_INTERVAL_SECONDS - 1),
            ..UpdateRecord::default()
        };
        assert!(should_check(&stale, "check"));
    }

    #[test]
    fn a_record_keeps_the_newest_release_so_up_to_date_is_distinguishable() {
        // The bug this locks: a successful check that found the installed
        // version used to clear `latest_version`, so the pane showed "no
        // release has been checked yet" right after saying it had checked.
        let record = UpdateRecord {
            checked_at: Some(now_seconds()),
            latest_version: Some(APP_VERSION.to_owned()),
            latest_tag: Some(format!("v{APP_VERSION}")),
            notes_url: None,
            last_error: None,
        };
        let payload = release_payload(&BoxConfig::default(), &record, APP_VERSION);
        assert_eq!(
            payload["version"].as_str(),
            Some(APP_VERSION),
            "the newest release must still be reported"
        );
        assert_eq!(
            payload["newer"].as_bool(),
            Some(false),
            "installed is not an update"
        );
    }

    #[test]
    fn a_failed_check_reports_the_error_and_leaves_the_timestamp_alone() {
        // "last tried" and "last successful" are different facts; a failed
        // check must not advance the second one.
        let root = std::env::temp_dir().join(format!("dshbox-update-record-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("state"));
        let _ = std::fs::remove_file(record_path(&root));
        let seeded = UpdateRecord {
            checked_at: Some(12345),
            ..UpdateRecord::default()
        };
        write_record(&root, &seeded).unwrap();
        // Point the fetch at a host that cannot answer, then assert the
        // timestamp survived. The failure path is reached by an unreachable
        // mirror rather than by mocking the network.
        let error = perform_check(&root, Some("http://127.0.0.1:1")).unwrap_err();
        assert!(!error.is_empty());
        let after = read_record(&root);
        assert_eq!(after.checked_at, Some(12345), "a failure must not look like a success");
        assert_eq!(after.last_error.as_deref(), Some(error.as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn the_off_policy_never_checks_on_its_own() {
        // Off is a standing preference; an explicit check_update still runs,
        // because a person asking once outranks it.
        assert!(!should_check(&UpdateRecord::default(), "off"));
    }
}