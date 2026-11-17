//! Framework-free storage layer for DSH Box.
//!
//! SQL backends for the document-store contract defined in
//! `box-foundation::collection`. Every persisted index (task queue today;
//! sealed templates, repository entries, resource map later) shares one
//! database at `<runtime>/state/dshbox.db` and one migration runner —
//! no per-domain SQL. Schema evolution is forward-only, gated by
//! `PRAGMA user_version`, matching the runtime-root schema policy.
//!
//! Dependency direction: `box-store → box-scheduler → box-foundation`. The
//! daemon and the desktop shell both open the SQLite backend through this
//! crate and inject it into their `TaskManager`; feature crates themselves
//! never touch this crate.

mod document;
mod import;

pub use document::SqliteDocumentStore;
pub use import::{open_document_store_for_paths, open_task_collection};
