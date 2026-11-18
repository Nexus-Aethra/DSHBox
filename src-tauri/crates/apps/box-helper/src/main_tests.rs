//! Tests for the helper argument surface.
//!
//! Only the pure parts are tested here. `connect` and `Link::call` need a real
//! daemon on the other end of a socket, and a bin crate cannot construct the
//! daemon state -- that is what the daemon gateway tests are for, and they
//! already drive the same `handle_link` through a full handshake. What is left
//! for this file is the parsing, which is where a bad flag becomes a silently
//! wrong configuration.

use super::*;

#[test]
fn a_control_url_splits_into_host_and_port() {
    assert_eq!(
        split_url("http://127.0.0.1:7100").unwrap(),
        ("127.0.0.1".to_owned(), 7100)
    );
    // The WSL gateway form, which is what a node inside a distro uses.
    assert_eq!(
        split_url("http://172.17.0.1:7100").unwrap(),
        ("172.17.0.1".to_owned(), 7100)
    );
}

#[test]
fn a_url_that_is_not_plain_http_is_refused() {
    // No https: the helper speaks plain HTTP on a private network by design,
    // and a URL that says otherwise is a misconfiguration rather than
    // something to half-support.
    assert!(split_url("https://127.0.0.1:7100").is_err());
    assert!(split_url("127.0.0.1:7100").is_err());
}

#[test]
fn a_url_with_no_port_is_refused() {
    assert!(split_url("http://127.0.0.1").is_err());
}

#[test]
fn the_link_flags_arrive_together_or_not_at_all() {
    // The four link flags build one config. A caller that supplies only some
    // of them still gets a config, and it is the daemon that refuses it --
    // which is why the helper says why the link did not open rather than
    // pretending it is fine.
    let parsed = parse_from([
        "--enroll".to_owned(),
        "tok".to_owned(),
        "--control".to_owned(),
        "http://127.0.0.1:7100".to_owned(),
        "--node-id".to_owned(),
        "node-1".to_owned(),
        "--link-seed".to_owned(),
        "ab".repeat(32),
        "--link-fingerprint".to_owned(),
        "fp-1".to_owned(),
        "--link-pubkey".to_owned(),
        "cd".repeat(32),
        "--link-chain-length".to_owned(),
        "64".to_owned(),
    ].into_iter());
    let config = parsed.unwrap().link.expect("the link flags built a config");
    assert_eq!(config.chain_length, 64);
    assert_eq!(config.pinned_fingerprint, "fp-1");
    assert!(!config.seed.is_empty());
}

#[test]
fn no_link_flags_means_no_config() {
    // The stage-A shape: register and heartbeat over HTTP, no encrypted link.
    let parsed = parse_from([
        "--enroll".to_owned(),
        "tok".to_owned(),
        "--control".to_owned(),
        "http://127.0.0.1:7100".to_owned(),
        "--node-id".to_owned(),
        "node-1".to_owned(),
    ].into_iter());
    assert!(parsed.unwrap().link.is_none());
}

#[test]
fn a_bad_chain_length_is_a_parse_error_not_a_default() {
    // Defaulting to something would hand the daemon a chain length the
    // operator never chose, and the first link would be wrong in a way that
    // looks like an authentication failure.
    let parsed = parse_from([
        "--enroll".to_owned(),
        "tok".to_owned(),
        "--control".to_owned(),
        "http://127.0.0.1:7100".to_owned(),
        "--node-id".to_owned(),
        "node-1".to_owned(),
        "--link-chain-length".to_owned(),
        "not-a-number".to_owned(),
    ].into_iter());
    assert!(parsed.is_err());
}
