//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    // `Read` arrives with `super::*`: the module already imports it for
    // BufReader, so repeating it here only earns an unused-import warning.
    use std::net::TcpListener;

    /// A daemon that accepts the connection and then says nothing must not hold
    /// the caller forever: a Tauri command thread would stay pending, and the
    /// CLI would look hung.
    #[test]
    fn a_silent_daemon_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and read the request, never reply.
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let client = RpcClient {
            port,
            token: "test".to_owned(),
        };
        let started = std::time::Instant::now();
        let error = client
            .exchange_within(
                serde_json::json!({ "token": "test", "method": "ping" }),
                Duration::from_millis(300),
            )
            .expect_err("a silent daemon is an error, not a value");
        assert!(
            error.contains("did not answer within"),
            "the error says what happened: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "it gives up on time, not when the daemon finally speaks"
        );
    }

    /// The same guard on the polling probe: a hung daemon answers "not yet" fast
    /// enough for the next poll.
    #[test]
    fn the_quick_probe_does_not_wait_for_a_stalled_daemon() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let client = RpcClient {
            port,
            token: "test".to_owned(),
        };
        let started = std::time::Instant::now();
        assert!(client.ping_quickly().is_err());
        assert!(
            started.elapsed() < PING_TIMEOUT + Duration::from_secs(1),
            "a poll returns inside its own budget"
        );
    }
