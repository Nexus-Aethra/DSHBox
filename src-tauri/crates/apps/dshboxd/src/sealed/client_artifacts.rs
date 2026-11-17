//! The client-artifact marker: has this Harness tree been built for this
//! commit and platform?
//!
//! The marker is what makes one build per Harness version serve every template
//! true: a tree without it for this platform is rebuilt rather than launched,
//! and a tree built for another OS never satisfies a guest.

use super::*;

/// Where a prepared base records that its client artifacts were built, and for
/// which commit and platform. Box writes it, so it can be trusted on a machine
/// other than the one that built the tree — an imported template carries
/// another platform's native addon.
pub(crate) const CLIENT_ARTIFACT_MARKER: &str = ".dsh-build/box-client-artifacts.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct ClientArtifactMarker {
    commit: String,
    platform: String,
}

/// The platform a built tree belongs to: the native addon and the web bundle
/// are only valid on the machine (and OS) that produced them.
fn build_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Record a completed client build. Written atomically, and only after the
/// build succeeded, so a half-written tree is never mistaken for a built one.
pub(crate) fn write_client_artifact_marker(harness: &Path, commit: &str) -> Result<(), String> {
    let path = harness.join(CLIENT_ARTIFACT_MARKER);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let marker = ClientArtifactMarker {
        commit: commit.to_owned(),
        platform: build_platform(),
    };
    let body = serde_json::to_vec_pretty(&marker).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, body).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

/// Whether this tree already carries client artifacts built for `commit` on
/// this platform.
pub(crate) fn client_artifacts_present(harness: &Path, commit: &str) -> bool {
    if !harness.join("apps/web/dist/index.html").is_file() {
        return false;
    }
    // The host starts this built entry, so a tree carrying the marker without
    // it (an interrupted build, or one that predates the entry requirement)
    // has to be rebuilt rather than trusted.
    if !harness.join("apps/cli/lib/bin.js").is_file() {
        return false;
    }
    let Ok(body) = fs::read_to_string(harness.join(CLIENT_ARTIFACT_MARKER)) else {
        return false;
    };
    match serde_json::from_str::<ClientArtifactMarker>(&body) {
        Ok(marker) => marker.commit == commit && marker.platform == build_platform(),
        Err(_) => false,
    }
}

/// Make sure a prepared base carries its client artifacts, building them in
/// place when it predates the build-at-base step. The base is the only tree
/// with `node_modules`, so it is the only place this can run cheaply.
pub(super) fn ensure_base_client_artifacts(
    base_directory: &Path,
    commit: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let harness = base_directory.join("harness");
    if client_artifacts_present(&harness, commit) {
        return Ok(());
    }
    let pnpm = resolve_toolchain("pnpm")?;
    let log_path = base_directory.join("prepare.log");
    task.update("Building client artifacts for this base", 22);
    task.log(
        "this prepared base was created before Box built client artifacts at base \
         time; building them once here (a few minutes) so every template from it is \
         ready to run",
    );
    let started = std::time::Instant::now();
    run_pnpm_command(
        &pnpm,
        &harness,
        ["run", "build"],
        &log_path,
        task,
        "prepared-base client build",
        None,
        Some(commit.to_owned()),
    )?;
    task.log(&format!(
        "client artifacts built in {}s",
        started.elapsed().as_secs()
    ));
    write_client_artifact_marker(&harness, commit)
}
