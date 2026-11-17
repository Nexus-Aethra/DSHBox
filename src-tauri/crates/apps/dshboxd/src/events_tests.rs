//! tests for events.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    /// Every node event, spelled out. A helper session token can subscribe
    /// to the event stream, so this list is the whole surface reachable from
    /// inside a guest: adding to it is a decision, not an accident.
    fn node_events() -> Vec<DaemonEvent> {
        vec![
            DaemonEvent::NodeRegistered {
                id: String::from("n1"),
                kind: String::from("wsl2"),
                platform: String::from("linux-x64"),
            },
            DaemonEvent::NodeStatus { id: String::from("n1"), status: String::from("online") },
            DaemonEvent::NodeLog {
                id: String::from("n1"),
                level: String::from("info"),
                line: String::from("hi"),
            },
        ]
    }

    #[test]
    fn the_node_surface_is_exactly_the_node_events() {
        for event in node_events() {
            assert!(event.is_node_event(), "{} must be a node event", event.event_name());
            assert_eq!(event.node_id(), Some("n1"));
        }
        let control = [
            DaemonEvent::TaskStage { task_id: String::from("t"), stage: String::from("s"), progress: 1 },
            DaemonEvent::TaskLog { task_id: String::from("t"), line: String::from("l") },
            DaemonEvent::TaskFinished { task_id: String::from("t"), status: String::from("done"), error: None },
            DaemonEvent::RollbackStarted { task_id: String::from("t") },
            DaemonEvent::RollbackFinished { task_id: String::from("t") },
            DaemonEvent::RollbackFailed { task_id: String::from("t"), error: String::from("e") },
            DaemonEvent::ResourceAdded { id: String::from("r"), r#type: String::from("credentials"), name: String::from("c") },
            DaemonEvent::ResourceRemoved { id: String::from("r") },
            DaemonEvent::ResourceUpdated { id: String::from("r"), status: String::from("deleted") },
            DaemonEvent::TagsFetched { tags: vec![] },
            DaemonEvent::UpdateState,
        ];
        for event in control {
            assert!(!event.is_node_event(), "{} must not reach a session subscriber", event.event_name());
            assert_eq!(event.node_id(), None);
        }
    }

    #[test]
    fn node_event_names_are_the_wire_names() {
        // The UI switches on these strings and the helper prints them; a
        // rename is a silent drop on both sides, so the wire spelling is
        // pinned here rather than remembered.
        let names: Vec<&str> = node_events().iter().map(|e| e.event_name()).collect();
        assert_eq!(names, vec!["node_registered", "node_status", "node_log"]);
    }
