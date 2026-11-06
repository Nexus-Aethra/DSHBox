//! Pins the build script's frontend checks.
//!
//! `build.rs` itself is never compiled by `cargo test`, so the decision logic
//! lives in `build_assets.rs` and this test crate includes it; the module's own
//! `#[cfg(test)] mod tests` is what actually runs here. A guard whose only
//! instrument is a build failure has no way to notice it started rejecting
//! bundles that are fine.

#[path = "../build_assets.rs"]
mod build_assets;

#[test]
fn the_dev_flag_answers_without_panicking() {
    // `embeds_frontend` is mostly exercised at build time. Here it only has to
    // read its environment without panicking and stay stable within a process -
    // the branch it takes depends on who is building, so nothing stronger is
    // meaningful in a test.
    assert_eq!(
        build_assets::embeds_frontend(),
        build_assets::embeds_frontend()
    );
}
