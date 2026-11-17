//! usable_url_tests for dispatch.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    fn record(state: host::HostState, authenticated: Option<&str>) -> host::ContainerHostRecord {
        host::ContainerHostRecord {
            id: "container-test".to_owned(),
            name: "test".to_owned(),
            template: None,
            profile: "web".to_owned(),
            host_pid: 1,
            host_pgid: 0,
            host_port: 1234,
            host_url: "http://127.0.0.1:1234".to_owned(),
            authenticated_url: authenticated.map(str::to_owned),
            started_at: 0,
            last_seen: 0,
            state,
            generation: 0,
            exit_status: None,
            exit_signal: None,
            unhealthy_count: 0,
            probe_count: 0,
        }
    }

    /// A booting host has no token yet. Handing out the bare URL there sends
    /// every caller into the 401 fence, which reads as "broken" rather than
    /// "not yet" -- so the reply must name the wait instead.
    #[test]
    fn starting_without_a_token_refuses_and_says_to_wait() {
        let error = usable_url(&record(host::HostState::Starting, None)).unwrap_err();
        assert!(error.contains("still starting"), "{error}");
        assert!(error.contains("retry"), "{error}");
        assert!(!error.contains("http"), "{error}");
    }

    /// The same absence, once the host is up, means an old runtime that never
    /// had a token. Refusing there would break it for no reason.
    #[test]
    fn ready_without_a_token_is_an_older_runtime_and_keeps_its_bare_url() {
        for state in [host::HostState::Ready, host::HostState::Running] {
            assert_eq!(
                usable_url(&record(state, None)).unwrap(),
                "http://127.0.0.1:1234"
            );
        }
    }

    #[test]
    fn a_token_is_preferred_in_every_state() {
        for state in [
            host::HostState::Starting,
            host::HostState::Ready,
            host::HostState::Running,
        ] {
            assert_eq!(
                usable_url(&record(state, Some("http://127.0.0.1:1234/?token=t"))).unwrap(),
                "http://127.0.0.1:1234/?token=t"
            );
        }
    }
