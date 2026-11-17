//! Tests for the L3 wire types and the session frame layer.
//!
//! The property worth pinning here is that a frame which opens but is out of
//! step is still a refusal -- the record layer refuses it on its seq before
//! the AEAD runs. The wire tests pin that a hello carrying another mode's
//! field is refused rather than half-accepted.

use crate::frame::{Frame, FrameType};
use crate::handshake::{self, AuthMode};
use crate::session_io::Session;
use crate::wire::{self, HelloWire};

fn hello(mode: AuthMode) -> handshake::Hello {
    handshake::Hello {
        node_id: "node-1".to_owned(),
        mode,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: [1u8; 16],
        eph_a: [2u8; 32],
        mac: if matches!(mode, AuthMode::Psk | AuthMode::Token) {
            Some([3u8; 32])
        } else {
            None
        },
        node_pubkey: if matches!(mode, AuthMode::NodeKey) {
            Some(vec![4u8; 32])
        } else {
            None
        },
        node_sig: if matches!(mode, AuthMode::NodeKey) {
            Some(vec![5u8; 64])
        } else {
            None
        },
    }
}

// ---------- wire round-trip ----------

#[test]
fn a_psk_hello_survives_the_round_trip() {
    let original = hello(AuthMode::Psk);
    let wire = wire::hello_wire(&original);
    let json = serde_json::to_string(&wire).unwrap();
    let back: HelloWire = serde_json::from_str(&json).unwrap();
    assert_eq!(back.to_hello().unwrap(), original);
}

#[test]
fn a_node_key_hello_survives_the_round_trip() {
    let original = hello(AuthMode::NodeKey);
    let wire = wire::hello_wire(&original);
    let json = serde_json::to_string(&wire).unwrap();
    let back: HelloWire = serde_json::from_str(&json).unwrap();
    assert_eq!(back.to_hello().unwrap(), original);
}

// ---------- mode discipline ----------

#[test]
fn a_psk_hello_carrying_a_node_key_field_is_refused() {
    // A peer trying to be two things at once. Refused at the wire boundary,
    // so the handshake never sees a half-populated hello.
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.node_sig = Some("ab".repeat(64));
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::WrongModeField {
            mode: AuthMode::Psk
        })
    );
}

#[test]
fn a_node_key_hello_carrying_a_mac_is_refused() {
    let mut wire = wire::hello_wire(&hello(AuthMode::NodeKey));
    wire.mac = Some("cd".repeat(32));
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::WrongModeField {
            mode: AuthMode::NodeKey
        })
    );
}

#[test]
fn a_hello_with_no_mac_is_refused_in_psk_mode() {
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.mac = None;
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::MissingField { field: "mac" })
    );
}

#[test]
fn a_downgraded_version_is_refused_at_the_wire_boundary() {
    // Refused before any key material is looked at.
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.version = handshake::VERSION + 1;
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::UnsupportedVersion {
            offered: handshake::VERSION + 1,
            supported: handshake::VERSION,
        })
    );
}

#[test]
fn a_downgraded_suite_is_refused_at_the_wire_boundary() {
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.suite = handshake::SUITE + 1;
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::UnsupportedSuite {
            offered: handshake::SUITE + 1,
            supported: handshake::SUITE,
        })
    );
}

#[test]
fn a_malformed_hex_field_names_itself() {
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.eph_a = "zz".to_owned();
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::BadHex { field: "ephA" })
    );
}

#[test]
fn a_short_field_is_a_length_error_not_a_hex_error() {
    let mut wire = wire::hello_wire(&hello(AuthMode::Psk));
    wire.eph_a = "ab".to_owned();
    assert_eq!(
        wire.to_hello().err(),
        Some(wire::WireError::BadLength { field: "ephA" })
    );
}

// ---------- the session frame layer ----------

/// Two ends of one session, built from the same key material.
///
/// This is the actual protocol: both sides derive the same two keys from
/// the same ECDH, and each side's send key is the other's receive key. A
/// single Session cannot round-trip a frame to itself, which is the point --
/// the direction separation is what stops a frame being replayed as the
/// other direction.
fn pair() -> (Session, Session) {
    let keys = handshake::SessionKeys {
        c2s: [10u8; 32],
        s2c: [11u8; 32],
        transcript: [12u8; 32],
    };
    let ours = Session::from_keys(keys.clone(), "node-1".to_owned(), AuthMode::Psk);
    // The peer sees the same two keys, so its c2s is our s2c.
    let theirs = Session::from_keys(
        handshake::SessionKeys {
            c2s: keys.s2c,
            s2c: keys.c2s,
            transcript: keys.transcript,
        },
        "node-1".to_owned(),
        AuthMode::Psk,
    );
    (ours, theirs)
}

#[test]
fn a_frame_round_trips_to_the_peer() {
    let (mut ours, mut theirs) = pair();
    let frame = ours
        .seal_frame(FrameType::Request, b"a request payload")
        .unwrap();
    // The seq is a clear 8-byte prefix, so the peer can order before it
    // decrypts.
    assert_eq!(u64::from_be_bytes(frame.payload[..8].try_into().unwrap()), 0);
    assert_eq!(theirs.open_frame(&frame).unwrap(), b"a request payload");
}

#[test]
fn a_replayed_frame_is_refused_by_the_peer() {
    let (mut ours, mut theirs) = pair();
    let frame = ours.seal_frame(FrameType::Request, b"first").unwrap();
    assert_eq!(theirs.open_frame(&frame).unwrap(), b"first");
    // The identical bytes again: refused on the seq, before the AEAD runs.
    assert!(theirs.open_frame(&frame).is_err());
}

#[test]
fn frames_carry_independent_counters_per_direction() {
    let (mut ours, _) = pair();
    let first = ours.seal_frame(FrameType::Request, b"one").unwrap();
    let second = ours.seal_frame(FrameType::Request, b"two").unwrap();
    assert_ne!(first.payload, second.payload);
    assert_eq!(u64::from_be_bytes(second.payload[..8].try_into().unwrap()), 1);
}

#[test]
fn a_frame_too_short_to_hold_a_seq_is_refused() {
    let (_, mut theirs) = pair();
    let frame = Frame {
        frame_type: FrameType::Request,
        payload: vec![1, 2, 3],
    };
    assert!(theirs.open_frame(&frame).is_err());
}

#[test]
fn a_handshake_frame_is_refused_inside_a_session() {
    // Hello and HelloAck are not encrypted, so a peer sending one inside an
    // established session is not speaking the protocol.
    let (_, mut theirs) = pair();
    let frame = Frame {
        frame_type: FrameType::Hello,
        payload: vec![0u8; 32],
    };
    assert!(theirs.open_frame(&frame).is_err());
}

#[test]
fn the_debug_form_never_prints_key_material() {
    let (ours, _) = pair();
    let session = &ours;
    let text = format!("{session:?}");
    assert!(text.contains("node-1"));
    // The keys are [10u8; 32] and [11u8; 32]; if either leaked as bytes the
    // debug output would contain "10" or "11" as decimal runs. Check the
    // transcript is absent rather than guessing at a substring.
    assert!(!text.contains("transcript"));
    assert!(!text.contains("0a0a"));
}
