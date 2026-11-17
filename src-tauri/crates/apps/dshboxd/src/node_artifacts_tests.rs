//! tests for node_artifacts.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

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
            base_id: "base-9c1f00d4a1b2c3d4".to_owned(),
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
        assert_eq!(row.id, "node-3f9a|base-9c1f00d4a1b2c3d4");
        assert_eq!(row.node_id, "node-3f9a");
        assert_eq!(row.base_id, "base-9c1f00d4a1b2c3d4");
        assert_eq!(row.platform, "linux-x86_64");
        assert_eq!(row.sha256, sha());
        assert_eq!(row.size_bytes, 1_327_000_000);
        assert!(row.built_at > 0);
        assert_eq!(index.list(None, None).unwrap(), vec![row.clone()]);
        assert_eq!(
            index.get("node-3f9a", "base-9c1f00d4a1b2c3d4").unwrap(),
            Some(row)
        );
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

        // The base id must be exactly what versions.rs:353 produces. These
        // are the spellings the first review's truth table showed the loose
        // validator accepting: a template id, a path-shaped digest, a package
        // target, non-ASCII, uppercase, a separator, and a short digest.
        for bad in [
            "sealed-9c1f00d4a1b2c3d4",
            "base-../../etc/passwd",
            "base-中文中文中文中文中文中文中文",
            "base-9C1F00D4A1B2C3D4",
            "base-9c1f|00d4a1b2c3d",
            "base-9c1f00d4",
            "base-9c1f00d4a1b2c3d45",
        ] {
            let mut report = sample("linux-x86_64");
            report.base_id = bad.to_owned();
            assert!(index.report(&wsl, &report).is_err(), "{bad:?} was accepted");
        }
        assert!(
            sample("linux-x86_64").base_id.len() == 21,
            "test fixture is 16 hex"
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

    /// A report is a small record, not a channel for arbitrary bytes. The cap
    /// is this RPC's own, not the tasks.params one from design section 3.5.
    #[test]
    fn a_text_field_longer_than_the_cap_is_refused() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let mut report = sample("linux-x86_64");
        report.archive = "a".repeat(MAX_TEXT_FIELD_BYTES + 1);
        let error = index.report(&wsl, &report).unwrap_err();
        assert!(error.contains("archive"), "{error}");
        assert!(index.list(None, None).unwrap().is_empty());
    }

    /// `get` answers for a pair, not merely for a concatenated id, so a row
    /// whose id and fields disagree is not silently returned as that pair.
    #[test]
    fn get_checks_the_fields_as_well_as_the_composite_id() {
        let index = ArtifactIndex::memory();
        let wsl = node("node-3f9a", "linux-x86_64");
        let mut row = index.report(&wsl, &sample("linux-x86_64")).unwrap();
        row.node_id = "node-forged".to_owned();
        index.collection.upsert(&[row]).unwrap();
        assert!(
            index
                .get("node-3f9a", &sample("linux-x86_64").base_id)
                .unwrap()
                .is_none(),
            "the row no longer describes that pair"
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
            index.get("node-3f9a", "base-9c1f00d4a1b2c3d4").unwrap(),
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
        other.base_id = "base-5e5e5e5e5e5e5e5e".to_owned();
        index.report(&a, &other).unwrap();
        index.report(&b, &sample("linux-x86_64")).unwrap();

        assert_eq!(index.list(None, None).unwrap().len(), 3);
        assert_eq!(index.list(Some("node-a"), None).unwrap().len(), 2);
        assert_eq!(index.list(Some("node-b"), None).unwrap().len(), 1);
        assert_eq!(
            index
                .list(None, Some("base-9c1f00d4a1b2c3d4"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            index
                .list(Some("node-b"), Some("base-5e5e5e5e5e5e5e5e"))
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

    /// The base id shape is coupled to the producer: versions.rs:353 formats
    /// template_content_hash as 16 lowercase hex. A change to either side must
    /// fail here, not silently produce rows the node's directory would reject.
    #[test]
    fn the_base_id_rule_accepts_what_the_producer_makes() {
        let produced = format!("base-{}", box_dsh_versions::template_content_hash("x"));
        assert!(is_base_id(&produced), "{produced:?}");
        assert_eq!(produced.strip_prefix("base-").unwrap().len(), 16);
    }
    #[test]
    fn the_scope_is_the_one_the_design_names() {
        assert_eq!(ARTIFACT_SCOPE, "node-artifacts");
    }
