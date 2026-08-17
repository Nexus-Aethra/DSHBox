//! `box-install-handlers` — single source of truth for turning an
//! `InstallSpec` into a fetched-and-staged source tree on disk.
//!
//! Handlers own only two things:
//!   1. **Fetch**: bring the package into a staging directory (cloning,
//!      downloading a tarball, running `pnpm pack`, or copying a
//!      local path). The returned path is the package's source root —
//!      the caller hands it to `import_into_repository` to land it in
//!      the shared plugin/skill/data store.
//!   2. **Profile install**: install into a container's profile
//!      `node_modules/` (or `skills/`, or `data/`), respecting
//!      `PathMode::Copy` vs `Link`, and rewriting `dsh.profile.bundles`
//!      so the container's `dsh` CLI sees the new plugin.
//!
//! The boxfile parser, the `dshbox` CLI, and the harness's own
//! `dsh plugin` layer all funnel through this crate so spec syntax,
//! fetch semantics, and post-install reconciliation can never drift
//! across surfaces.

pub mod spec;
pub mod handler;
pub mod profile_scan;

pub use handler::{handler_for, InstallHandler, InstallError, InstallOutcome};
pub use profile_scan::{scan_profile_plugins, ProfilePluginEntry};
pub use spec::{parse_spec, GitHost, InstallSpec, PathMode, SpecError, WorkspaceProtocol};