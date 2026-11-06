use std::env;
use std::path::Path;

/// Count the hashed frontend assets that `index.html` references, verifying
/// each one exists on disk.
///
/// A desktop binary whose bundle carries no JavaScript still starts, still
/// answers the daemon, and renders a blank window - which reads as "that
/// feature was never implemented" rather than "this build never packaged the
/// frontend".
fn embedded_asset_count(dist: &Path, html: &str) -> Result<usize, String> {
    let mut count = 0usize;
    for segment in html.split('"') {
        let relative = segment.trim_start_matches('/');
        if !relative.starts_with("assets/") {
            continue;
        }
        if !dist.join(relative).is_file() {
            return Err(format!(
                "src-tauri/dist/index.html references {relative}, but that file is not on disk"
            ));
        }
        if relative.ends_with(".js") || relative.ends_with(".css") {
            count += 1;
        }
    }
    if count == 0 {
        return Err(
            "src-tauri/dist/index.html references no assets/*.js or assets/*.css".to_string(),
        );
    }
    Ok(count)
}

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Vite writes `src-tauri/dist` (see vite.config.ts) and `frontendDist` is
    // `dist`, so tauri_build validates and embeds exactly those bytes. Copying
    // the repo-root `dist/` here - as this script used to - silently replaced a
    // fresh frontend with whatever stale bundle happened to sit at the root.

    // `custom-protocol` is what switches the window from `devUrl`
    // (http://localhost:1420) to the embedded bundle. `pnpm tauri build` sets it
    // automatically; a hand-run `cargo build --release -p dshbox` does not, and
    // yields a release binary with no frontend at all that still launches - it
    // just points at a dev server nobody runs. 0.1.10 was installed exactly that
    // way: the CLI, the daemon and every RPC worked while the window stayed
    // blank, so the guard is a build failure that names the command to use.
    let profile = env::var("PROFILE").unwrap_or_default();
    let custom_protocol = env::var("CARGO_FEATURE_CUSTOM_PROTOCOL").is_ok();
    if profile == "release" && !custom_protocol {
        panic!(
            "a release build of dshbox must enable `custom-protocol`, or the desktop embeds \
             no frontend and opens the unreachable dev URL. Build it with \
             `cargo build --release -p dshbox --features custom-protocol`, or run \
             `pnpm tauri build`, which passes the feature for you."
        );
    }

    // Embedding also requires the bundle to be real: a `dist/` whose index.html
    // points at assets that are not there, or references none at all, produces
    // the same blank window and would otherwise pass silently.
    if custom_protocol {
        let dist = manifest_dir.join("dist");
        let html_path = dist.join("index.html");
        let html = std::fs::read_to_string(&html_path).unwrap_or_else(|err| {
            panic!(
                "custom-protocol is on but {} cannot be read ({err}); \
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
