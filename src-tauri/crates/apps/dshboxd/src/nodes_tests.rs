//! tests for nodes.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use crate::events::DaemonEvents;

    fn manager() -> NodeManager {
        NodeManager::load(
            &BoxPaths { config: std::path::PathBuf::from("/nonexistent"), runtime: None },
            "http://127.0.0.1:1".to_owned(),
            Arc::new(DaemonEvents::new()),
        )
    }

    /// Design section 2.7: the platform is a small closed set in the
    /// `{OS}-{ARCH}` spelling. These are the values the reviewers' live probes
    /// showed the old enroll accepting, which made I1 pass on free text.
    #[test]
    fn enroll_refuses_any_platform_that_is_not_os_arch() {
        let m = manager();
        for bad in [
            "banana",
            "linux-x64", // a package target, not a platform
            "LINUX-X86_64",
            "",
            "win-x64",
            "linux-x86_64 ",
        ] {
            let error = m.enroll("wsl2", bad, vec![], None).unwrap_err();
            assert!(error.contains("platform"), "{bad:?}: {error}");
        }
        for good in [
            "linux-x86_64",
            "linux-aarch64",
            "windows-x86_64",
            "windows-aarch64",
            "macos-x86_64",
            "macos-aarch64",
        ] {
            assert!(m.enroll("wsl2", good, vec![], None).is_ok(), "{good:?}");
        }
    }

    /// The helper reports the platform it actually runs on, and that value
    /// wins over the one the supply adapter typed at enroll. Without this,
    /// the daemon validates the adapter's spelling, not the machine's.
    #[test]
    fn register_records_the_platform_the_helper_reported() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "windows-x86_64", vec![], None).unwrap();
        let (_, _, _, _) = m
            .register(&grant, None, Some("linux-x86_64".to_owned()))
            .unwrap();
        assert_eq!(m.get(&id).unwrap().unwrap().platform, "linux-x86_64");
    }

    /// An invalid helper-reported platform is refused, not folded into the
    /// grant's value: the grant was already validated at enroll.
    #[test]
    fn register_refuses_an_invalid_helper_platform() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], None).unwrap();
        let error = m
            .register(&grant, None, Some("banana".to_owned()))
            .unwrap_err();
        assert!(error.contains("platform"), "{error}");
        assert!(
            m.get(&id).unwrap().is_none(),
            "a refused register writes no record"
        );
    }

    /// No helper platform keeps the grant's (validated) value, so a helper
    /// built before this field existed can still register.
    #[test]
    fn register_without_a_helper_platform_keeps_the_grants_value() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], None).unwrap();
        m.register(&grant, None, None).unwrap();
        assert_eq!(m.get(&id).unwrap().unwrap().platform, "linux-x86_64");
    }

    #[test]
    fn node_status_is_lowercase_on_the_wire() {
        assert_eq!(serde_json::to_string(&NodeStatus::Online).unwrap(), "\"online\"");
        assert_eq!(serde_json::to_string(&NodeStatus::Registering).unwrap(), "\"registering\"");
    }

    #[test]
    fn host_node_is_built_in_online_and_protected() {
        let m = manager();
        let host = m.get(HOST_NODE_ID).unwrap().unwrap();
        assert!(host.host_node);
        assert_eq!(host.status, NodeStatus::Online);
        assert!(m.deregister(HOST_NODE_ID).is_err());
        assert!(m.heartbeat(HOST_NODE_ID, m.current_epoch()).is_err());
    }

    #[test]
    fn enroll_register_and_scoped_session() {
        let m = manager();
        let (grant, id, epoch, _) = m.enroll("wsl2", "linux-x86_64", vec!["exec".to_owned()], None).unwrap();
        // A second node, so the bearer can be shown not to resolve for it.
        let (other_grant, other_id, _, _) =
            m.enroll("wsl2", "linux-x86_64", vec![], None).unwrap();
        assert!(m.register(&other_grant, None, None).is_ok());
        let (session, hb) = {
            let (rid, token, ep, hb) = m.register(&grant, Some("0.1.0".to_owned()), None).unwrap();
            assert_eq!(rid, id);
            assert_eq!(ep, epoch);
            (token, hb)
        };
        assert_eq!(hb, HEARTBEAT_SECONDS);
        // A second use of the same one-shot grant must fail.
        assert!(m.register(&grant, None, None).is_err());
        // The session resolves to its own node at the current epoch…
        // session_node_for is what /rpc and /events actually gate on: the
        // identity has to travel with the scope, not just a yes/no.
        assert_eq!(m.session_node_for(Some(&session), epoch).as_deref(), Some(id.as_str()));
        // …but not for a stale epoch, not without a bearer, and not for a
        // different node. The last one is unreachable by construction today
        // -- the lookup is keyed by bearer -- so this is insurance against a
        // refactor that keys it some other way.
        assert_eq!(m.session_node_for(Some(&session), epoch + 1), None);
        assert_eq!(m.session_node_for(None, epoch), None);
        assert_ne!(
            m.session_node_for(Some(&session), epoch).as_deref(),
            Some(other_id.as_str()),
            "a bearer must not resolve to another node"
        );
        // Whitelist: a session token cannot reach control-plane methods.
        assert!(NodeManager::is_node_method("node_heartbeat"));
        // B1: a helper must be able to report a base it built, or its very
        // first report is a 403 (design section 2.3). The index read stays on
        // the control-plane side of the split.
        assert!(NodeManager::is_node_method("node_artifact_report"));
        assert!(!NodeManager::is_node_method("node_artifact_list"));
        assert!(!NodeManager::is_node_method("list_containers"));
        assert!(!NodeManager::is_node_method("delete_container"));
        let node = m.get(&id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Online);
        assert_eq!(node.kind, "wsl2");
        assert_eq!(node.platform, "linux-x86_64");
    }

    #[test]
    fn heartbeat_updates_last_seen_and_stale_epoch_is_rejected() {
        let m = manager();
        let (grant, id, epoch, _) = m.enroll("libkrun", "linux-x86_64", vec![], None).unwrap();
        let (session, _, _, _) = m.register(&grant, None, None).unwrap();
        let _ = session;
        assert_eq!(m.heartbeat(&id, epoch).unwrap(), NodeStatus::Online);
        assert!(m.heartbeat(&id, epoch + 1).is_err());
    }

    #[test]
    fn sweep_marks_only_stale_non_host_nodes_offline() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("docker", "linux-x86_64", vec![], None).unwrap();
        m.register(&grant, None, None).unwrap();
        // Force the record into the past without sleeping.
        let mut node = m.get(&id).unwrap().unwrap();
        node.last_seen = Some(now_seconds() - OFFLINE_AFTER_SECONDS - 1);
        m.collection.upsert(&[node]).unwrap();
        assert_eq!(m.sweep(), 1);
        assert_eq!(m.get(&id).unwrap().unwrap().status, NodeStatus::Offline);
        // The host node never expires.
        assert_eq!(m.get(HOST_NODE_ID).unwrap().unwrap().status, NodeStatus::Online);
        assert_eq!(m.sweep(), 0);
    }

    #[test]
    fn restart_demotes_surviving_online_nodes_but_keeps_records() {
        // Sessions are memory-only, so a persisted `online` is a lie the moment
        // the previous process is gone. Loading a fresh manager must correct it
        // at once rather than after a sweep interval.
        // A real runtime dir, so both managers open the same document store
        // and the second one genuinely reads what the first wrote.
        // A UUID, not now_seconds(): a wall-clock second is coarse enough
        // that a re-run inside the same second reuses the previous
        // directory, and this root is a document store another test may
        // still be reading.
        let root = std::env::temp_dir().join(format!(
            "dshboxd-nodes-restart-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = BoxPaths { config: root.join("config.json"), runtime: Some(root.clone()) };
        // A block, so both managers -- and with them the SQLite connection
        // they hold on `state/dshbox.db` -- are dropped before the root is
        // removed. Removing first would fail on Windows while the handle is
        // open, and the `let _ =` would hide it.
        {
            let first = NodeManager::load(&paths, "http://127.0.0.1:1".to_owned(), Arc::new(DaemonEvents::new()));
            let (grant, id, _, _) = first.enroll("wsl2", "linux-x86_64", vec![], Some("survivor".to_owned())).unwrap();
            first.register(&grant, None, None).unwrap();
            assert_eq!(first.get(&id).unwrap().unwrap().status, NodeStatus::Online);

            let reloaded = NodeManager::load(&paths, "http://127.0.0.1:2".to_owned(), Arc::new(DaemonEvents::new()));
            let record = reloaded.get(&id).unwrap().expect("record survives the restart");
            assert_eq!(record.status, NodeStatus::Offline);
            assert!(record.enrolled_at.is_some(), "the record is kept, not deleted");
            // The host node is ours, not a survivor, and stays online.
            assert_eq!(reloaded.get(HOST_NODE_ID).unwrap().unwrap().status, NodeStatus::Online);
            // Demoting is idempotent.
            assert_eq!(reloaded.demote_orphans_after_restart(), 0);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn constant_time_eq_handles_lengths_and_empty() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn node_record_round_trips() {
        let record = NodeRecord {
            id: "n1".to_owned(),
            kind: "wsl2".to_owned(),
            platform: "linux-x86_64".to_owned(),
            status: NodeStatus::Draining,
            capabilities: vec!["exec".to_owned()],
            control_url: Some("http://127.0.0.1:1".to_owned()),
            session_epoch: 7,
            enrolled_at: Some(1),
            last_seen: Some(2),
            helper_version: Some("0.1.0".to_owned()),
            backend_ref: None,
            host_node: false,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: NodeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
        assert!(json.contains("\"hostNode\":false"));
        assert!(json.contains("\"sessionEpoch\":7"));
    }

    #[test]
    fn host_id_is_reserved_at_enroll() {
        // Registering under the host id would overwrite the built-in record
        // with host_node:false, and the protection that guards the host node
        // assumes the id is not a helper's to take.
        let m = manager();
        let error = m
            .enroll("wsl2", "linux-x86_64", vec![], Some(HOST_NODE_ID.to_owned()))
            .unwrap_err();
        assert!(error.contains("reserved"), "unexpected error: {error}");
        let host = m.get(HOST_NODE_ID).unwrap().unwrap();
        assert!(host.host_node);
        assert_eq!(host.kind, "host");
    }

    #[test]
    fn a_bad_grant_reads_the_same_whatever_went_wrong() {
        // Unknown, already used and expired must be indistinguishable: the
        // register path is reachable without a credential, so telling them
        // apart tells a stranger which guesses were closer.
        let m = manager();
        let unknown = m.register("no-such-grant", None, None).unwrap_err();
        let used = {
            let grant = m.enroll("wsl2", "linux-x86_64", vec![], Some("g2".into())).unwrap().0;
            m.register(&grant, None, None).unwrap();
            m.register(&grant, None, None).unwrap_err()
        };
        let expired = {
            let grant = m.enroll("wsl2", "linux-x86_64", vec![], Some("g3".into())).unwrap().0;
            if let Ok(mut grants) = m.grants.lock() {
                if let Some(g) = grants.get_mut(&grant) {
                    g.expires_at = now_seconds() - 1;
                }
            }
            m.register(&grant, None, None).unwrap_err()
        };
        assert_eq!(unknown, used);
        assert_eq!(used, expired);
    }

    #[test]
    fn register_promotes_registering_to_online() {
        // The record must never claim online before its session exists, so
        // the intermediate state has to be real and settled on success.
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("promo".into())).unwrap();
        let (_rid, bearer, _ep, _hb) = m.register(&grant, None, None).unwrap();
        assert_eq!(m.get(&id).unwrap().unwrap().status, NodeStatus::Online);
        // session_still_matches is what an open stream re-checks, so a live
        // session must satisfy it -- and the bearer is the one register
        // handed back, not a second registration of the one-shot grant.
        assert!(m.session_still_matches(&id, &bearer), "a successful register leaves a live session");
    }

    #[test]
    fn a_deregistered_node_has_no_session_left() {
        // An open SSE stream asks exactly this to notice a revocation.
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("revoked".into())).unwrap();
        let (_rid, bearer, _ep, _hb) = m.register(&grant, None, None).unwrap();
        assert!(m.session_still_matches(&id, &bearer));
        m.deregister(&id).unwrap();
        assert!(!m.session_still_matches(&id, &bearer), "revocation must revoke");
    }

    #[test]
    fn sweep_expires_registering_and_purges_grants() {
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("stuck".into())).unwrap();
        m.register(&grant, None, None).unwrap();
        let mut rec = m.get(&id).unwrap().unwrap();
        rec.status = NodeStatus::Registering;
        rec.last_seen = Some(now_seconds() - OFFLINE_AFTER_SECONDS - 1);
        m.collection.upsert(&[rec]).unwrap();
        assert_eq!(m.sweep(), 1);
        assert_eq!(m.get(&id).unwrap().unwrap().status, NodeStatus::Offline);
        let live = m.enroll("wsl2", "linux-x86_64", vec![], Some("purge".into())).unwrap().0;
        if let Ok(mut grants) = m.grants.lock() {
            if let Some(g) = grants.get_mut(&live) {
                g.expires_at = now_seconds() - 1;
            }
        }
        m.sweep();
        assert!(!m.grants.lock().unwrap().contains_key(&live));
    }

    #[test]
    fn a_second_session_replaces_the_first_and_revokes_it() {
        // Two grants for one node id. The second register replaces the
        // session, so a stream opened with the first token must be told it is
        // no longer the live credential — checking "a session exists for this
        // node" would keep saying yes forever.
        let m = manager();
        let (g1, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("dup".into())).unwrap();
        let (_, first, _, _) = m.register(&g1, None, None).unwrap();
        assert!(m.session_still_matches(&id, &first));
        let (g2, id2, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("dup".into())).unwrap();
        assert_eq!(id, id2);
        let (_, second, ep2, _) = m.register(&g2, None, None).unwrap();
        assert_ne!(first, second);
        assert!(m.session_still_matches(&id, &second), "the new bearer is live");
        assert!(!m.session_still_matches(&id, &first), "the replaced bearer is not");
        // And the per-node shortcut would still say yes for the OLD bearer's
        // node, which is why a stream must resolve per credential instead.
        assert!(m.session_node_for(Some(&second), ep2).is_some());
        // The old bearer resolves to nothing, which is what a stream needs.
        assert!(m.session_node_for(Some(&first), ep2).is_none());
    }

    #[test]
    fn deregister_between_session_and_promote_leaves_the_node_deleted() {
        // The window between writing the record and promoting it is real; a
        // deregister landing inside it must not be undone by the promote.
        let m = manager();
        let (grant, id, _, _) = m.enroll("wsl2", "linux-x86_64", vec![], Some("win".into())).unwrap();
        m.register(&grant, None, None).unwrap();
        m.deregister(&id).unwrap();
        assert!(m.get(&id).unwrap().is_none(), "a promote must not resurrect a deleted node");
    }

    #[test]
    fn register_records_the_epoch_and_enrol_time_the_promote_guards_on() {
        let m = manager();
        let (grant, id, epoch, _) = m
            .enroll("wsl2", "linux-x86_64", vec![], Some("meta".into()))
            .unwrap();
        m.register(&grant, None, None).unwrap();
        let node = m.get(&id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Online);
        assert_eq!(node.session_epoch, epoch);
        assert!(node.enrolled_at.is_some(), "the promote needs this to identify its own record");
    }
