//! Bundling glue for the `@deepseek-ai/dsh-box-context` Cordis plugin.
//!
//! The npm package lives beside this crate as `dsh-box-context/`; this
//! library only exposes constants the Rust build pipeline (resource
//! packaging, runtime vendor, and lifecycle wiring) reads to keep names
//! in lockstep across both halves.

/// Cordis plugin id used by every entry and patch overlay.
pub const PLUGIN_ID: &str = "dsh-box-context";

/// Scoped npm package name; must match `dsh-box-context/package.json#name`
/// verbatim because DSH resolves it through Node's standard `require`
/// machinery.
pub const PLUGIN_PACKAGE: &str = "@deepseek-ai/dsh-box-context";

/// Default prompt-context order. Sandbox policy owns 110, approval policy
/// 115, subagent delegation 120; 130 leaves room for one more authoritative
/// section without colliding with the 100–199 tool-guidance band.
pub const DEFAULT_ORDER: u32 = 130;
