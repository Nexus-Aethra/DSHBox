// Embed the build-batch stamp (`src-tauri/.build-stamp`, epoch seconds written by
// scripts/prepare-server.mjs on every real rebuild) so the daemon can report which
// build it came from over RPC. The desktop client embeds the same file and restarts
// stale daemons when they disagree.
use std::path::{Path, PathBuf};

fn main() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release = std::env::var("PROFILE")
        .map(|profile| profile == "release")
        .unwrap_or(false);
    let stamp_path = walk_up(&crate_dir, ".build-stamp");

    let stamp = match &stamp_path {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.display());
            std::fs::read_to_string(path).unwrap_or_default()
        }
        None => {
            // `.build-stamp` is generated and gitignored, so a fresh clone has none.
            // A *release* build must not embed an empty stamp silently: the only
            // symptom is a client that reports a stale server, which reads like a
            // broken upgrade rather than a missing step. A dev or test build has to
            // work out of the box, so it warns and carries on.
            let hint = "run `pnpm server:prepare` (scripts/prepare-server.mjs) to write src-tauri/.build-stamp";
            if release {
                panic!(
                    "cannot find .build-stamp above {}; {hint}",
                    crate_dir.display()
                );
            }
            println!(
                "cargo:warning=no .build-stamp above {}; {hint}",
                crate_dir.display()
            );
            String::new()
        }
    };

    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
    println!(
        "cargo:rustc-env=DSHBOX_APP_VERSION={}",
        app_version(&crate_dir)
    );
}

/// Walk up from the crate directory until `marker` exists.
///
/// Counting parent hops broke once, when `dshboxd` moved into `crates/apps/`: the
/// build embedded an **empty** stamp and the only symptom was a client that kept
/// reporting a stale server. Returns `None` when nothing above the crate has the
/// marker, so the caller decides what a missing marker means.
fn walk_up(start: &Path, marker: &str) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(marker);
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// The *application* version, which is not this crate's version.
//
// `dshboxd` is an internal crate and keeps its own 0.1.0, while the version a user
// sees lives in `src-tauri/Cargo.toml` / `tauri.conf.json`. The updater compares
// releases against that version, and using `CARGO_PKG_VERSION` here made a 0.1.9
// install report itself as 0.1.0 and therefore claim every published release was an
// update.
fn app_version(crate_dir: &Path) -> String {
    let mut dir = crate_dir.to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if text.lines().any(|line| line.trim() == "name = \"dshbox\"") {
                println!("cargo:rerun-if-changed={}", manifest.display());
                for line in text.lines() {
                    if let Some(rest) = line.trim().strip_prefix("version = ") {
                        return rest.trim().trim_matches('"').to_owned();
                    }
                }
                return String::new();
            }
        }
        if !dir.pop() {
            return String::new();
        }
    }
}
