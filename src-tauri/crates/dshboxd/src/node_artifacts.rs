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

/// A base id is base-<digest> and nothing else.
///
/// The spelling matters: a prepared base is base-<digest> while a template is
/// sealed-<digest>, and rows keyed by the template spelling would multiply by
/// the number of templates built on one base (design doc section 11).
pub fn is_base_id(value: &str) -> bool {
    match value.strip_prefix("base-") {
        Some(digest) => {
            !digest.is_empty() && !digest.contains('|') && !digest.chars().any(char::is_whitespace)
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
        Ok(self
            .collection
            .load_all()?
            .into_iter()
            .find(|row| row.id == id))
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
                "baseId must be base-<digest> with no '|' or whitespace: {:?}",
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
        if report.source_commit.trim().is_empty() {
            return Err("sourceCommit is required".to_owned());
        }
        if report.archive.trim().is_empty() {
            return Err("archive is required".to_owned());
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
mod tests {
    use super::*;
    use crate::nodes::NodeStatus;

    fn node(id: &str, platform: &str) -> NodeRecord {
        NodeRecord {
            id: id.to_owned(),
            kind: "wsl2".to_owned(),
            platform: platform.to_owned(),
            status: NodeStatus::Online,
            capabilities: vec!["build".to_owned()],
            control_url: Some("http://127.0.0.1:9000".to_owned()),
            session_epoch: 1,
            enrolled_at: Some(1),
            last_seen: Some(1),
            helper_version: Some("0.1.10".to_owned()),
            backend_ref: Some("dshbox-node-a".to_owned()),
            host_node: false,
        }
    }

    fn sha() -> String {
        format!("9f2c{}", "a".repeat(60))
    }

    fn sample(platform: &str) -> ArtifactReport {
        ArtifactReport {
            base_id: "base-9c1f00d4".to_owned(),
            source_commit: "0d1f500".to_owned(),
            platform: platform.to_owned(),
            archive: "/var/dshbox/artifacts/base-9c1f00d4-task-1.tar".to_owned(),
            sha256: sha(),
            size_bytes: 1_327_000_000,
        }
    }

    #[test]
    fn a_report_records_one_row_keyed_by_node_and_base() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let row = index.report(&wsl, &sample("linux-x86_64")).unwrap();
        assert_eq!(row.id, "node-3f9a|base-9c1f00d4");
        assert_eq!(row.node_id, "node-3f9a");
        assert_eq!(row.base_id, "base-9c1f00d4");
        assert_eq!(row.platform, "linux-x86_64");
        assert_eq!(row.sha256, sha());
        assert_eq!(row.size_bytes, 1_327_000_000);
        assert!(row.built_at > 0);
        assert_eq!(index.list(None, None).unwrap(), vec![row.clone()]);
        assert_eq!(index.get("node-3f9a", "base-9c1f00d4").unwrap(), Some(row));
    }

    /// Invariant I1, as a bug this repository has already written once: a row
    /// whose platform is the host's makes a Linux base look built on Windows,
    /// and every later selection then skips a build that never happened.
    #[test]
    fn a_row_whose_platform_is_not_the_nodes_is_refused() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let error = index.report(&wsl, &sample("windows-x86_64")).unwrap_err();
        assert!(error.contains("windows-x86_64"), "{error}");
        assert!(error.contains("linux-x86_64"), "{error}");
        assert!(
            index.list(None, None).unwrap().is_empty(),
            "and the refused report writes nothing"
        );
    }

    #[test]
    fn shape_checks_refuse_a_report_that_cannot_be_trusted() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");

        let mut report = sample("linux-x86_64");
        report.base_id = "sealed-9c1f00d4".to_owned();
        assert!(index
            .report(&wsl, &report)
            .unwrap_err()
            .contains("base-<digest>"));

        let mut report = sample("linux-x86_64");
        report.base_id = "base-9c1f|00d4".to_owned();
        assert!(
            index.report(&wsl, &report).is_err(),
            "the key separator is reserved"
        );

        let mut report = sample("linux-x86_64");
        report.sha256 = report.sha256.to_uppercase();
        assert!(index.report(&wsl, &report).unwrap_err().contains("sha256"));

        let mut report = sample("linux-x86_64");
        report.sha256.truncate(63);
        assert!(index.report(&wsl, &report).unwrap_err().contains("sha256"));

        let mut report = sample("linux-x86_64");
        report.size_bytes = 0;
        assert!(index
            .report(&wsl, &report)
            .unwrap_err()
            .contains("sizeBytes"));

        let mut report = sample("linux-x86_64");
        report.source_commit.clear();
        assert!(index
            .report(&wsl, &report)
            .unwrap_err()
            .contains("sourceCommit"));

        let mut report = sample("linux-x86_64");
        report.archive = "   ".to_owned();
        assert!(index.report(&wsl, &report).unwrap_err().contains("archive"));

        assert!(
            index.list(None, None).unwrap().is_empty(),
            "and none of the refused reports wrote a row"
        );
    }

    /// Invariant I4: one node, one base, one row. A rebuild for the same pair
    /// replaces it, which is what a stale-platform retry relies on.
    #[test]
    fn a_second_report_for_the_same_pair_overwrites_the_row() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let first = index.report(&wsl, &sample("linux-x86_64")).unwrap();
        let mut retry = sample("linux-x86_64");
        retry.archive = "/var/dshbox/artifacts/base-9c1f00d4-task-2.tar".to_owned();
        let second = index.report(&wsl, &retry).unwrap();

        let rows = index.list(None, None).unwrap();
        assert_eq!(rows.len(), 1, "one node, one base, one row");
        assert_eq!(rows[0].id, first.id);
        assert_eq!(rows[0].archive, retry.archive);
        assert_eq!(
            index.get("node-3f9a", "base-9c1f00d4").unwrap(),
            Some(second)
        );
    }

    #[test]
    fn list_filters_by_node_and_base() {
        let index = ArtifactIndex::memory();
        let a = node("node-a", "linux-x86_64");
        let b = node("node-b", "linux-x86_64");
        index.report(&a, &sample("linux-x86_64")).unwrap();
        let mut other = sample("linux-x86_64");
        other.base_id = "base-5e5e5e5e".to_owned();
        index.report(&a, &other).unwrap();
        index.report(&b, &sample("linux-x86_64")).unwrap();

        assert_eq!(index.list(None, None).unwrap().len(), 3);
        assert_eq!(index.list(Some("node-a"), None).unwrap().len(), 2);
        assert_eq!(index.list(Some("node-b"), None).unwrap().len(), 1);
        assert_eq!(index.list(None, Some("base-9c1f00d4")).unwrap().len(), 2);
        assert_eq!(
            index
                .list(Some("node-b"), Some("base-5e5e5e5e"))
                .unwrap()
                .len(),
            0
        );
    }

    /// The wire shape is pinned the same way the node record's is: a renamed
    /// field would otherwise leave both suites green and a helper unable to read
    /// the answer.
    #[test]
    fn the_row_speaks_camel_case_on_the_wire() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let row = index.report(&wsl, &sample("linux-x86_64")).unwrap();
        let value = serde_json::to_value(&row).unwrap();
        for key in [
            "id",
            "nodeId",
            "baseId",
            "sourceCommit",
            "platform",
            "archive",
            "sha256",
            "sizeBytes",
            "builtAt",
        ] {
            assert!(value.get(key).is_some(), "{key} is on the wire: {value}");
        }
        let back: NodeArtifact = serde_json::from_value(value).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn the_scope_is_the_one_the_design_names() {
        assert_eq!(ARTIFACT_SCOPE, "node-artifacts");
    }
}
