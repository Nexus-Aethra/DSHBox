//! The frontend-bundle checks `build.rs` runs before a desktop build is allowed
//! to produce a binary.
//!
//! They live in their own file because a build script is not compiled by
//! `cargo test`: keeping the decision logic here lets
//! `tests/frontend_embed_guard.rs` include it and pin the behaviour that has to
//! hold on both sides of the guard - a complete bundle passes, and the two ways
//! of shipping no frontend at all do not.

use std::env;
use std::path::Path;

/// Whether this build embeds the frontend instead of pointing the window at
/// `devUrl` (`http://localhost:1420`).
///
/// `tauri`'s own build script derives `dev = !custom-protocol` from *its*
/// features and publishes it as `cargo:dev`; because that crate declares
/// `links = "Tauri"`, the value reaches the dependent's build script as
/// `DEP_TAURI_DEV`. That flag - not this package's `custom-protocol` alias - is
/// what decides the question: `pnpm tauri build` passes `--features
/// tauri/custom-protocol` and never enables the alias, and `tauri`'s `default`
/// features omit `custom-protocol` entirely, so a bare `cargo build --release`
/// embeds no asset at all. 0.1.10 was installed exactly that way: the CLI, the
/// daemon and every RPC worked while the window stayed blank.
pub fn embeds_frontend() -> bool {
    match env::var("DEP_TAURI_DEV") {
        Ok(dev) => dev != "true",
        // Older `tauri` without that metadata, or a future one that stops
        // publishing it: fall back to our alias. Note the direction - if `tauri`
        // ever drops `cargo:dev`, `tauri build` goes back to panicking here, so
        // this branch is a compatibility net, not a second source of truth.
        Err(_) => env::var("CARGO_FEATURE_CUSTOM_PROTOCOL").is_ok(),
    }
}

/// Count the frontend assets `index.html` references, verifying each one exists
/// on disk.
///
/// A desktop binary whose bundle carries no JavaScript still starts, still
/// answers the daemon, and renders a blank window - which reads as "that feature
/// was never implemented" rather than "this build never packaged the frontend".
/// A stylesheet alone is not a frontend either, so at least one `assets/*.js` is
/// required.
pub fn embedded_asset_count(dist: &Path, html: &str) -> Result<usize, String> {
    let mut js = 0usize;
    let mut css = 0usize;
    // Vite's own output quotes with double quotes and writes root-absolute
    // paths, but a project that sets `base: './'` writes `./assets/...` and
    // other tooling emits single quotes; accepting all of them keeps the guard
    // from failing a bundle that is in fact complete.
    for segment in html.split(|c| c == '"' || c == '\'') {
        let relative = segment.trim();
        let relative = relative.trim_start_matches("./");
        let relative = relative.trim_start_matches('/');
        let relative = relative.split(['?', '#']).next().unwrap_or(relative);
        if !relative.starts_with("assets/") {
            continue;
        }
        let file = dist.join(relative);
        if !file.is_file() {
            return Err(format!(
                "src-tauri/dist/index.html references {relative}, but that file is not on disk"
            ));
        }
        if relative.ends_with(".js") {
            js += 1;
        } else if relative.ends_with(".css") {
            css += 1;
        }
    }
    if js == 0 {
        return Err(format!(
            "src-tauri/dist/index.html carries no assets/*.js reference ({css} stylesheet(s)); \
             look at that file - an empty bundle and an asset URL shape this check cannot read \
             both look like this"
        ));
    }
    Ok(js + css)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Once;

    /// A throwaway `dist/` holding one `.js` asset, plus whatever `index.html`
    /// the test needs.
    fn fixture(name: &str, html: &str) -> PathBuf {
        // One base directory per test process, wiped once on first use, so a run
        // leaves nothing behind for the next one.
        static CLEANED: Once = Once::new();
        let dir = env::temp_dir().join(format!("dshbox-guard-{}-{name}", std::process::id()));
        CLEANED.call_once(|| {
            let base = env::temp_dir().join(format!("dshbox-guard-{}", std::process::id()));
            let _ = fs::remove_dir_all(base);
        });
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("assets/index-ABCDEFGH.js"), "console.log(1)").unwrap();
        fs::write(dir.join("assets/index-ABCDEFGH.css"), "body{}").unwrap();
        fs::write(dir.join("index.html"), html).unwrap();
        dir
    }

    #[test]
    fn accepts_vites_real_output_shape() {
        let dir = fixture(
            "vite",
            "<script type=\"module\" crossorigin src=\"/assets/index-ABCDEFGH.js\"></script>\
             <link rel=\"stylesheet\" crossorigin href=\"/assets/index-ABCDEFGH.css\">",
        );
        assert_eq!(
            embedded_asset_count(&dir, &fs::read_to_string(dir.join("index.html")).unwrap()),
            Ok(2)
        );
    }

    #[test]
    fn accepts_single_quotes_relative_base_and_query_strings() {
        for html in [
            "<script src='/assets/index-ABCDEFGH.js'></script>",
            "<script src=\"./assets/index-ABCDEFGH.js\"></script>",
            "<script src=\"/assets/index-ABCDEFGH.js?v=1\"></script>",
        ] {
            let dir = fixture("shape", html);
            assert!(
                embedded_asset_count(&dir, &fs::read_to_string(dir.join("index.html")).unwrap())
                    .is_ok(),
                "rejected a complete bundle: {html}"
            );
        }
    }

    #[test]
    fn rejects_a_bundle_with_no_javascript() {
        let dir = fixture(
            "css-only",
            "<link rel=\"stylesheet\" href=\"/assets/index-ABCDEFGH.css\">",
        );
        let html = fs::read_to_string(dir.join("index.html")).unwrap();
        let err = embedded_asset_count(&dir, &html).unwrap_err();
        assert!(err.contains("no assets/*.js"), "{err}");
    }

    #[test]
    fn rejects_a_reference_that_is_not_on_disk() {
        let dir = fixture(
            "missing",
            "<script src=\"/assets/index-ZZZZZZZZ.js\"></script>",
        );
        let html = fs::read_to_string(dir.join("index.html")).unwrap();
        let err = embedded_asset_count(&dir, &html).unwrap_err();
        assert!(err.contains("not on disk"), "{err}");
    }

    /// The checked-in copy of a real Vite build. The live `dist/` cannot serve
    /// this purpose: `pnpm build` has not necessarily run, and a test that
    /// silently returns when a directory is missing is a test that never runs in
    /// CI - which is exactly where the shipped bundle is produced.
    #[test]
    fn the_real_vite_output_shape_passes() {
        let dist = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("frontend");
        let html = fs::read_to_string(dist.join("index.html")).unwrap();
        assert_eq!(embedded_asset_count(&dist, &html), Ok(2));
    }
}
