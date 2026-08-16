use std::path::Path;

fn main() {
    tauri_build::build();
    // Embed the build-batch stamp (epoch seconds, written by
    // scripts/prepare-server.mjs on every real daemon rebuild) so the
    // desktop client can match itself against the daemon over RPC.
    let stamp_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".build-stamp");
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    let stamp = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
}
