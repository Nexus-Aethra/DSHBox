//! Container resources: the state a container persists outside its code —
//! chat history, provider credentials, and whatever a plugin writes under the
//! profile — plus extraction and injection so that state can be moved between
//! containers or baked into a template.
//!
//! A *kind* says where that state lives and how an injection may treat an
//! existing destination. Kinds come from three places, in order of authority:
//!
//! 1. built in (`sessions`, `credentials`),
//! 2. declared by a plugin package (`dshbox.resources` in its `package.json`),
//! 3. inferred from the paths a plugin's shipped code composes.
//!
//! Only (1) and (2) are trusted; an inferred path is a candidate the caller
//! shows to the user before anything is extracted.

pub mod discover;
pub mod kinds;
pub mod record;
pub mod transfer;

#[cfg(test)]
mod tests;

pub use discover::{discover, Discovered, Scope};
pub use kinds::{builtin, builtins, Conflict, Kind, ResolvedKind, Shape};
pub use record::{build_id, collection, payload_dir, resources_root, ResourceRecord};
pub use transfer::{entries_at, extract, inject, Extracted, Injected};
