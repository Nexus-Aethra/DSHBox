//! Data Scheduler — unified resource map + soft-delete + dual-queue async hard-delete.
//!
//! All managed resources (plugins, templates, containers) are registered in a
//! single `resource-map.json`. `remove` never touches the filesystem directly:
//! it removes the active entry and enqueues the path on the fast deletion
//! queue. The background `DeletionWorker` drains the fast queue on every map
//! write and polls the slow queue every 60 s; entries that exhaust their
//! retry budget are promoted to `permanent_failures` and left on disk with a
//! diagnostic log line.
//!
//! See `docs/specs/data-scheduler.md` for the full design.

pub mod map;
pub mod queue;
pub mod worker;

pub use map::*;
pub use queue::*;
pub use worker::DeletionWorker;
