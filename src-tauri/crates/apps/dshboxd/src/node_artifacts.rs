//! The node artifact index (roadmap stage B1).
//!
//! A prepared Harness base builds once per (node, base) and stays on that node's
//! disk. This module owns the control plane's *index* of those builds, never the
//! bytes: the archive is an opaque handle the node understands, and there is no
//! verified-at column, because a control plane that cannot stat the path would
//! only be recording a guess of its own (design doc section 2.2, invariant I8).
//!
//! What is checked here is what a control plane can honestly check: the shape of
//! what the node reports, plus the one cross-record fact it holds itself -- the
//! platform of the node that reported it (invariant I1).

use crate::nodes::NodeRecord;
use box_foundation::collection::Collection;
use box_foundation::{now_seconds, BoxPaths, BoxResult};
use box_store::open_document_store_for_paths;
use serde::{Deserialize, Serialize};

/// New scope, same store. Adding a scope is a new row, not a migration: the
/// schema version stays at v1 (design doc section 5).
pub const ARTIFACT_SCOPE: &str = "node-artifacts";

/// A text field on a report is a name or an opaque handle, not a payload.
/// The design caps `tasks.params` at 4 KiB (section 3.5); this is a separate
/// boundary with its own cap, so a broken helper cannot write an unbounded row.
const MAX_TEXT_FIELD_BYTES: usize = 4096;

/// One prepared base as it exists on one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeArtifact {
    /// "<nodeId>|<baseId>". A document id is one TEXT column, so a composite key
    /// has to be a single string, and the separator is reserved: a base id may
    /// not contain it (see `is_base_id`).
    pub id: String,
    pub node_id: String,
    /// base-<digest>: the prepared base, not the sealed-<digest> template.
    pub base_id: String,
    pub source_commit: String,
    /// The node's platform, always -- this column is what invariant I1 is about.
    pub platform: String,
    /// Opaque handle on the node's disk. Never opened here, never joined.
    pub archive: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// When the control plane *received* the report (now_seconds), not when the
    /// node finished the build: the report carries no build timestamp, and this
    /// index makes no claim about the node's clock (design section 2.2).
    pub built_at: u64,
}

pub fn artifact_key(record: &NodeArtifact) -> &str {
    record.id.as_str()
}

pub fn artifact_id(node_id: &str, base_id: &str) -> String {
    format!("{node_id}|{base_id}")
}

/// What a node sends after it has written the tar and hashed it.
///
/// Every field defaults, so a report missing one is refused by the validation
/// below with a message naming it, instead of failing to deserialize with a
/// message naming a serde field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtifactReport {
    pub base_id: String,
    pub source_commit: String,
    pub platform: String,
    pub archive: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// A base id is `base-<digest>` and nothing else.
///
/// Two spellings matter. A prepared base is `base-<digest>` while a template is
/// `sealed-<digest>`, so rows keyed by the template spelling would multiply by
/// the number of templates built on one base (design doc section 11). And the
/// digest is exactly what `versions.rs:353` produces (16 lowercase hex digits)
/// and `prepared_base_dir` will accept (`is_safe_identifier`): a looser shape
/// here would let a report name a base the node's own directory resolution
/// refuses — the control plane would hold a row for a base that cannot exist.
pub fn is_base_id(value: &str) -> bool {
    match value.strip_prefix("base-") {
        Some(digest) => {
            digest.len() == 16
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        None => false,
    }
}

/// 64 lowercase hex digits. Uppercase is refused rather than folded: the same
/// digest in two spellings would be two cache entries.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The artifact rows, read through the document store like every other index.
pub struct ArtifactIndex {
    collection: Collection<NodeArtifact>,
}

impl ArtifactIndex {
    pub fn load(paths: &BoxPaths) -> Self {
        let collection = match open_document_store_for_paths(paths) {
            Ok(store) => Collection::new(store, ARTIFACT_SCOPE, artifact_key),
            Err(error) => {
                tracing::warn!("node artifact index using in-memory store: {error}");
                Collection::memory(ARTIFACT_SCOPE, artifact_key)
            }
        };
        Self { collection }
    }

    /// In-memory index, for tests that never leave one process.
    pub fn memory() -> Self {
        Self {
            collection: Collection::memory(ARTIFACT_SCOPE, artifact_key),
        }
    }

    pub fn list(
        &self,
        node_id: Option<&str>,
        base_id: Option<&str>,
    ) -> BoxResult<Vec<NodeArtifact>> {
        let mut rows = self.collection.load_all()?;
        rows.retain(|row| {
            node_id.is_none_or(|id| row.node_id == id) && base_id.is_none_or(|id| row.base_id == id)
        });
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows)
    }

    pub fn get(&self, node_id: &str, base_id: &str) -> BoxResult<Option<NodeArtifact>> {
        let id = artifact_id(node_id, base_id);
        // The composite id is a single string, so a row whose fields disagree
        // with it must not be returned as if it were that pair: `list` filters
        // on the fields, and the two answers would otherwise diverge (design
        // section 2.2; the separator has no escape in this encoding).
        Ok(self
            .collection
            .load_all()?
            .into_iter()
            .find(|row| row.id == id && row.node_id == node_id && row.base_id == base_id))
    }

    /// Record a build a node reports as done, or refuse it.
    ///
    /// Every rejection is a refusal rather than a repair: a row the control
    /// plane cannot vouch for is worse than no row, because this index is what
    /// later decides whether to skip a build that takes minutes.
    pub fn report(
        &self,
        node: &NodeRecord,
        report: &ArtifactReport,
    ) -> Result<NodeArtifact, String> {
        if !is_base_id(&report.base_id) {
            return Err(format!(
                "baseId must be base-<16 lowercase hex digits>: {:?}",
                report.base_id
            ));
        }
        if !is_sha256(&report.sha256) {
            return Err(format!(
                "sha256 must be 64 lowercase hex digits: {:?}",
                report.sha256
            ));
        }
        if report.size_bytes == 0 {
            return Err("sizeBytes must be greater than zero".to_owned());
        }
        for (name, value) in [
            ("baseId", report.base_id.as_str()),
            ("sourceCommit", report.source_commit.as_str()),
            ("platform", report.platform.as_str()),
            ("archive", report.archive.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} is required"));
            }
            if value.len() > MAX_TEXT_FIELD_BYTES {
                return Err(format!(
                    "{name} exceeds {MAX_TEXT_FIELD_BYTES} bytes: {}",
                    value.len()
                ));
            }
        }
        // Invariant I1, and the reason it is a rejection: an artifact's platform
        // is the node's. A helper that reports the platform it *runs on* (a WSL2
        // node is a Linux guest, but a careless caller reports the Windows host)
        // leaves a row that makes every later selection skip a build that never
        // happened on that platform.
        if report.platform != node.platform {
            return Err(format!(
                "artifact platform {:?} does not match node {:?} platform {:?}",
                report.platform, node.id, node.platform
            ));
        }
        // Design section 2.2 also refuses a report from a removed node. That
        // status arrives with the tombstone in B1-b2, so there is no variant to
        // test yet -- the one piece of the section still missing, noted rather
        // than silently skipped.
        let record = NodeArtifact {
            id: artifact_id(&node.id, &report.base_id),
            node_id: node.id.clone(),
            base_id: report.base_id.clone(),
            source_commit: report.source_commit.clone(),
            platform: report.platform.clone(),
            archive: report.archive.clone(),
            sha256: report.sha256.clone(),
            size_bytes: report.size_bytes,
            built_at: now_seconds(),
        };
        self.collection
            .upsert(&[record.clone()])
            .map_err(|error| error.to_string())?;
        Ok(record)
    }
}

#[cfg(test)]
#[path = "node_artifacts_tests.rs"]
mod tests;
