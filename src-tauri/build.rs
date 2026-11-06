#[path = "build_assets.rs"]
mod build_assets;

use std::env;
use std::path::Path;

use build_assets::{embedded_asset_count, embeds_frontend};

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Vite writes `src-tauri/dist` (see vite.config.ts) and `frontendDist` is
    // `dist`, so tauri_build validates and embeds exactly those bytes. Copying
    // the repo-root `dist/` here - as this script used to - silently replaced a
    // fresh frontend with whatever stale bundle happened to sit at the root.

    // Two separate ways to ship a desktop binary with no frontend, both of which
    // start fine, answer the daemon, and render a blank window (0.1.10 was
    // installed that way). `embeds_frontend` is `tauri`'s own dev/production
    // switch; see `build_assets.rs` for why it is the flag to read and the
    // `custom-protocol` alias is not.
    // `PROFILE = "release"` is only what makes a bad build fail *early* - the
    // invariant is `embeds_frontend`. A bench or a custom profile that inherits
    // release is therefore not covered by the first check, but it is still
    // covered by the second whenever it embeds.
    let profile = env::var("PROFILE").unwrap_or_default();
    let embedding = embeds_frontend();
    if profile == "release" && !embedding {
        panic!(
            "this release build of dshbox embeds no frontend: `tauri` is in dev mode, so the \
             window would open the dev URL http://localhost:1420 - nothing serves that in an \
             installed layout - and render blank. Build it with `cargo build --release -p \
             dshbox --features tauri/custom-protocol`, or run `pnpm tauri build`, which passes \
             that feature for you."
        );
    }

    // The other shape: embedding is on, but the bundle it would embed is empty or
    // incomplete. Checking it here is what turns `pnpm build` into a hard
    // prerequisite of a release build instead of a step people forget.
    if embedding {
        let dist = manifest_dir.join("dist");
        let html_path = dist.join("index.html");
        println!("cargo:rerun-if-changed={}", html_path.display());
        let html = std::fs::read_to_string(&html_path).unwrap_or_else(|err| {
            panic!(
                "this build embeds the frontend, but {} cannot be read ({err}); \
                 run `pnpm build` first",
                html_path.display()
            )
        });
        match embedded_asset_count(&dist, &html) {
            Ok(count) => println!(
                "cargo:warning=embedding frontend from src-tauri/dist ({count} hashed assets)"
            ),
            Err(reason) => panic!("{reason}; run `pnpm build` before building the desktop"),
        }
    }

    tauri_build::build();

    let stamp_path = manifest_dir.join(".build-stamp");
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    let stamp = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
}
