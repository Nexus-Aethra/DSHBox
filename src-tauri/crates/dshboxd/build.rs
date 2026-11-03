// Embed the build-batch stamp (`src-tauri/.build-stamp`, epoch seconds
// written by scripts/prepare-server.mjs on every real rebuild) so the
// daemon can report which build it came from over RPC. The desktop client
// embeds the same file and restarts stale daemons when they disagree.
use std::path::Path;

fn main() {
    let stamp_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.build-stamp");
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    let stamp = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
    println!("cargo:rustc-env=DSHBOX_APP_VERSION={}", app_version());
}
// The *application* version, which is not this crate's version.
//
// `dshboxd` is an internal crate and keeps its own 0.1.0, while the version a
// user sees lives in `src-tauri/Cargo.toml` / `tauri.conf.json`. The updater
// compares releases against that version, and using `CARGO_PKG_VERSION` here
// made a 0.1.9 install report itself as 0.1.0 and therefore claim every
// published release was an update.
fn app_version() -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(&manifest).unwrap_or_default();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("version = ") {
            return rest.trim().trim_matches('"').to_owned();
        }
    }
    String::new()
}