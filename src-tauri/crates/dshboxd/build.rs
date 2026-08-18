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
}
