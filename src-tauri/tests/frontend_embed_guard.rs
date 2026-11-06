//! Pins the build script's frontend checks.
//!
//! `build.rs` itself is never compiled by `cargo test`, so the decision logic
//! lives in `build_assets.rs` and this test crate includes it; the module's own
//! `#[cfg(test)] mod tests` is what actually runs here. A guard whose only
//! instrument is a build failure has no way to notice it started rejecting
//! bundles that are fine.

#[path = "../build_assets.rs"]
mod build_assets;
