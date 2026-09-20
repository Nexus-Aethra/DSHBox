use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Vite writes `src-tauri/dist` (see vite.config.ts) and `frontendDist` is
    // `dist`, so tauri_build validates and embeds exactly those bytes. Copying
    // the repo-root `dist/` here — as this script used to — silently replaced a
    // fresh frontend with whatever stale bundle happened to sit at the root.
    tauri_build::build();

    let stamp_path = manifest_dir.join(".build-stamp");
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    let stamp = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
}
