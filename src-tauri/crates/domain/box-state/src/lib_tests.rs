//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::collections::BTreeMap;

    fn task(status: &str) -> TaskRecord {
        TaskRecord {
            id: "1".to_owned(),
            kind: "install".to_owned(),
            resource_keys: vec!["toolchain:node".to_owned()],
            status: status.to_owned(),
            stage: status.to_owned(),
            progress: 42,
            stages: Vec::new(),
            created_at: 1,
            started_at: None,
            finished_at: None,
            log_path: String::new(),
            error: Some("broken".to_owned()),
            params: serde_json::json!({}),
            cancel_requested: false,
            rollback_error: None,
            node_id: None,
            claimed_by: None,
            claim_session: None,
            idempotency_key: None,
        }
    }
    fn toolchain() -> ToolchainStatus {
        ToolchainStatus {
            id: "node".to_owned(),
            name: "Node.js".to_owned(),
            system_version: Some("v1".to_owned()),
            managed_version: None,
        }
    }

    #[test]
    fn task_lock_marks_its_resource_busy() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![task("running")],
        );
        assert_eq!(
            manager.resource("toolchain:node").unwrap().unwrap().health,
            ResourceHealth::Busy
        );
    }

    #[test]
    fn failed_task_is_reported_on_resource() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![task("failed")],
        );
        assert_eq!(
            manager.resource("toolchain:node").unwrap().unwrap().health,
            ResourceHealth::Failed
        );
    }


    /// `miss` is a lost claim, not a failed build, but the resource must not
    /// read as Ready: the UI would otherwise offer a container whose
    /// preparation never completed (design 2.5). Removing "miss" from the
    /// match arm must turn this red.
    #[test]
    fn a_missed_task_is_reported_as_failed_on_its_resource() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![task("miss")],
        );
        assert_eq!(
            manager.resource("toolchain:node").unwrap().unwrap().health,
            ResourceHealth::Failed
        );
    }

    #[test]
    fn bootstrap_uses_fresh_facts() {
        let manager = ResourceStateManager::bootstrap(
            &BoxConfig::default(),
            vec![toolchain()],
            vec![],
            vec![],
            vec![],
        );
        manager.refresh_all(&BoxConfig::default(), vec![], vec![], vec![]);
        assert!(manager.snapshot().unwrap().resources.is_empty());
        let _ = BTreeMap::<String, String>::new();
    }
