//! wire_format_tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use super::{task_state_machine_definition, TaskState};

    /// Every state this crate can produce, beside the spelling it takes on the
    /// wire. Each test below iterates this list, so a variant added to the enum
    /// cannot be left behind by one of them; and the literal is the contract --
    /// `domain.ts`'s `TaskStatus` and the agent plugin compare against these
    /// exact strings, so "it is lower case and parses back" would not be a pin.
    const ALL_STATES: [(TaskState, &str); 9] = [
        (TaskState::Queued, "queued"),
        (TaskState::Running, "running"),
        (TaskState::Succeeded, "succeeded"),
        (TaskState::Failed, "failed"),
        (TaskState::Cancelled, "cancelled"),
        (TaskState::Interrupted, "interrupted"),
        (TaskState::Miss, "miss"),
        (TaskState::RollingBack, "rollingback"),
        (TaskState::RolledBack, "rolledback"),
    ];

    fn name(state: &TaskState) -> String {
        state.to_string()
    }

    /// The spelling a caller sees on the wire, pinned here because a task poll
    /// is written against it. The Rust spelling is not it: a caller comparing
    /// against `Succeeded` matches nothing, and a finished task is then never
    /// recognised as finished.
    #[test]
    fn task_state_serialises_in_lower_case() {
        for (state, wire) in ALL_STATES.iter() {
            let encoded = serde_json::to_string(state).expect("a task state encodes");
            assert_eq!(encoded, format!("\"{wire}\""), "{state:?} on the wire");
        }
    }

    /// Both spellings that reach a record parse back to the state they came
    /// from: serde writes `rollingback`, `transition_to` writes `rolling_back`,
    /// and a lost claim adds `miss`. A reader that knows only one of them
    /// silently reclassifies a finished task.
    #[test]
    fn every_spelling_parses_back_to_its_state() {
        for (state, wire) in ALL_STATES.iter() {
            assert_eq!(
                TaskState::from_known(wire).map(|parsed| name(&parsed)),
                Some(name(state)),
                "{wire} parses back"
            );
            assert_eq!(
                TaskState::from_known(name(state).as_str()).map(|parsed| name(&parsed)),
                Some(name(state)),
                "{state:?} through Display"
            );
        }
    }

    /// `task_state_machine_definition` is served over RPC and is what a client
    /// renders. A state missing from it is a state no client can show; a flag
    /// that disagrees with the predicates is worse than no flag; and a
    /// `transitions` list that drifted from `valid_transitions` is a diagram
    /// that lies about what can follow what.
    #[test]
    fn the_advertised_state_machine_matches_the_enum() {
        let definition = task_state_machine_definition();
        let states = definition["states"].as_array().expect("states is an array");
        let advertised: Vec<String> = states
            .iter()
            .map(|state| {
                state["name"]
                    .as_str()
                    .expect("every state is named")
                    .to_owned()
            })
            .collect();
        let expected: Vec<String> = ALL_STATES.iter().map(|(state, _)| name(state)).collect();
        assert_eq!(
            advertised, expected,
            "the RPC's state list drifted from the enum"
        );

        for (state, _) in ALL_STATES.iter() {
            let label = name(state);
            let entry = states
                .iter()
                .find(|entry| entry["name"].as_str() == Some(label.as_str()))
                .expect("every state is advertised");
            assert_eq!(
                entry["terminal"].as_bool(),
                Some(state.is_terminal()),
                "{label} terminal"
            );
            assert_eq!(
                entry["active"].as_bool(),
                Some(state.is_active()),
                "{label} active"
            );
            let mut advertised_edges: Vec<String> = entry["transitions"]
                .as_array()
                .expect("transitions is an array")
                .iter()
                .map(|edge| edge.as_str().expect("an edge is a string").to_owned())
                .collect();
            let mut real_edges: Vec<String> = state.valid_transitions().iter().map(name).collect();
            advertised_edges.sort();
            real_edges.sort();
            assert_eq!(advertised_edges, real_edges, "{label} transitions");
        }
    }
