//! One extracted resource, and where its payload lives.
//!
//! Records go through the shared document store (`Collection`) so the daemon is
//! the only writer; payloads are content directories under
//! `<runtime>/resources/<id>/payload/`.

use crate::kinds::Shape;
use box_foundation::collection::{Collection, DocumentStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRecord {
    /// `<kind>-<name>`, sanitized.
    pub id: String,
    pub kind: String,
    pub name: String,
    /// Container the payload came out of.
    pub source_container: String,
    /// Container-relative path it came from, so an injection can go back to the
    /// same place by default.
    pub source_path: String,
    pub digest: String,
    pub bytes: u64,
    pub files: u64,
    pub secret: bool,
    /// Payload shape at extraction time, so an injection knows whether it may
    /// merge entry by entry.
    #[serde(default = "default_shape")]
    pub shape: Shape,
    /// Entry depth at extraction time.
    #[serde(default = "default_depth")]
    pub entry_depth: u8,
    /// The package it belongs to, when a plugin owned the resource.
    #[serde(default)]
    pub plugin: Option<String>,
    pub created_at: u64,
}

fn default_shape() -> Shape {
    Shape::Opaque
}

fn default_depth() -> u8 {
    1
}

pub fn record_key(record: &ResourceRecord) -> &str {
    &record.id
}

pub fn collection(store: Arc<dyn DocumentStore>) -> Collection<ResourceRecord> {
    Collection::new(store, "resources", record_key)
}

pub fn resources_root(runtime: &Path) -> PathBuf {
    runtime.join("resources")
}

pub fn payload_dir(runtime: &Path, id: &str) -> PathBuf {
    resources_root(runtime).join(id).join("payload")
}

/// A filesystem-safe id; `sessions` + `--home-wpp--` must not become a path.
pub fn build_id(kind: &str, name: &str) -> String {
    let clean = |value: &str| -> String {
        let mapped: String = value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = mapped.trim_matches(['-', '.']).to_string();
        if trimmed.is_empty() {
            "resource".to_owned()
        } else {
            trimmed.to_owned()
        }
    };
    format!("{}-{}", clean(kind), clean(name))
}
