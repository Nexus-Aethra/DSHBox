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
    // The stamp is only valid next to the *application* manifest, which is where
    // prepare-server writes it. Looking for the bare filename anywhere above the
    // crate would accept a stray stamp from an unrelated directory (a packed
    // checkout, an enclosing scratch tree) and embed a build id that names another
    // batch -- the exact symptom this stamp exists to catch, wearing a green light.
    let stamp_path = app_manifest_dir(&crate_dir).map(|dir| dir.join(".build-stamp"));

    // Watch the stamp whenever its location is known, existing or not. Emitting
    // this only when the file was already there left a dev build (which warns and
    // carries on) with an empty DSHBOX_BUILD_STAMP for as long as the crate itself
    // stayed unchanged: the stamp appearing is exactly the event that has to make
    // cargo run this script again.
    if let Some(path) = &stamp_path {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let stamp = match stamp_path.as_deref().map(|path| (path, path.exists())) {
        Some((path, true)) => std::fs::read_to_string(path).unwrap_or_default(),
        _ => {
            // `.build-stamp` is generated and gitignored, so a fresh clone has none.
            // A *release* build must not embed an empty stamp silently: the only
            // symptom is a client that reports a stale server, which reads like a
            // broken upgrade rather than a missing step. A dev or test build has to
            // work out of the box, so it warns and carries on.
            let hint = "run `pnpm server:prepare` (scripts/prepare-server.mjs) to write src-tauri/.build-stamp";
            if release {
                panic!(
                    "cannot find .build-stamp beside the dshbox manifest above {}; {hint}",
                    crate_dir.display()
                );
            }
            println!(
                "cargo:warning=no .build-stamp beside the dshbox manifest above {}; {hint}",
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

// The directory holding the *application* manifest: the Cargo.toml whose package
// is `dshbox`. Counting parent hops broke once, when `dshboxd` moved into
// `crates/apps/`; naming the package instead survives any further move, and it is
// the same anchor `app_version` reads, so the stamp and the version it is embedded
// beside can never come from two different checkouts.
fn app_manifest_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if text.lines().any(|line| line.trim() == "name = \"dshbox\"") {
                return Some(dir);
            }
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
    let Some(dir) = app_manifest_dir(crate_dir) else {
        return String::new();
    };
    let manifest = dir.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return String::new();
    };
    println!("cargo:rerun-if-changed={}", manifest.display());
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("version = ") {
            return rest.trim().trim_matches('"').to_owned();
        }
    }
    String::new()
}
