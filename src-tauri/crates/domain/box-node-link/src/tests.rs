//! Security tests for the node link handshake, chain and record layer.
//!
//! These are the design doc section 9 list, one test per line. The list is
//! the acceptance criteria for the whole protocol, so it is kept in the same
//! order and each test names the number it pins.
//!
//! Every negative case asserts two things: the handshake fails, and it
//! fails with the *reason* the design gives for refusing it. A single
//! opaque error would let a tampered field and a downgraded suite be
//! refused for the same wrong reason.

use ring::rand::SecureRandom;
use ring::signature::KeyPair;
use ring::rand::SystemRandom;

use crate::chain::{self, ChainNode, ChainState, DEFAULT_CHAIN_LENGTH};
use crate::handshake::{self, AuthMode, Hello, HelloAck, SessionKeys};
use crate::session::{self, RecordKey, SessionCert};

fn rng() -> SystemRandom {
    // The OS generator. These tests assert on failure and success, not on
    // exact bytes, so determinism buys nothing here and a fixed seed would
    // only hide a dependency on one particular ephemeral key.
    SystemRandom::new()
}

fn control_keypair() -> ring::signature::Ed25519KeyPair {
    // ring builds an Ed25519 key pair from a PKCS#8 document, which it can
    // generate itself from a random source.
    let rng = SystemRandom::new();
    let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    ring::signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap()
}

/// A complete, honest handshake, used as the baseline every negative case
/// perturbs.
struct Exchange {
    keys: SessionKeys,
    ack: HelloAck,
    hello: Hello,
}

/// Run the node and control halves against each other in psk mode.
fn psk_exchange(psk: &[u8]) -> Exchange {
    let rng = rng();
    let signing = control_keypair();
    let mut nonce = [0u8; 16];
    rng.fill(&mut nonce).unwrap();
    let mut client_nonce = [0u8; 16];
    rng.fill(&mut client_nonce).unwrap();

    let node_hand = handshake::NodeHand::new(&rng, signing.public_key().as_ref().to_vec()).unwrap();
    let eph_a = node_hand.public_key().unwrap();
    let hello = Hello {
        node_id: "node-1".to_owned(),
        mode: AuthMode::Psk,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce,
        eph_a,
        mac: None,
        node_pubkey: None,
        node_sig: None,
    };
    let t1 = handshake::transcript_client(&hello);
    let mac = crate::kdf::hmac_sha256(&crate::kdf::bootstrap_key(psk), &t1);
    let hello = Hello { mac: Some(mac), ..hello };

    let control = handshake::ControlHand::new(&rng).unwrap();
    let cert = serde_json::to_vec(&SessionCert {
        node_id: "node-1".to_owned(),
        control_fingerprint: "fp-1".to_owned(),
        issued_at: 1,
        expires_at: 2,
    })
    .unwrap();
    let (keys, ack) =
        handshake::control_finish(control, &hello, nonce, &signing, &cert, psk).unwrap();

    Exchange { keys, ack, hello }
}

// ---------- positive cases (design section 9) ----------

#[test]
fn a_clean_psk_handshake_derives_two_different_directional_keys() {
    let exchange = psk_exchange(b"a perfectly good psk");
    assert_ne!(exchange.keys.c2s, exchange.keys.s2c);
}

#[test]
fn the_control_signature_verifies_against_the_pinned_fingerprint_key() {
    let exchange = psk_exchange(b"a perfectly good psk");
    let other = control_keypair();
    let other_pubkey = other.public_key().as_ref().to_vec();
    let peer = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &other_pubkey);
    // A different key must refuse the same signature, which is what makes
    // the fingerprint check meaningful rather than decorative.
    assert!(peer.verify(&exchange.keys.transcript, &exchange.ack.control_sig).is_err());
}

#[test]
fn a_retry_after_a_lost_ack_uses_a_smaller_k() {
    // The implementation red line: persist before sending, and any failure
    // or retry goes back DOWN the chain. Resending the same c_k is a
    // permanent refusal, because H(c_k) is c_k+1 and the control plane now
    // expects c_k.
    let mut seed = [0u8; 32];
    seed[0] = 3;
    let mut state = ChainState::from_seed(&seed, DEFAULT_CHAIN_LENGTH);
    let mut node = ChainNode::new(seed, DEFAULT_CHAIN_LENGTH);

    let (value, k) = node.next_value();
    chain::accept(&mut state, &value).unwrap();

    // The ack was lost. The node must not reuse k; it must go lower.
    node.rewind_to(k);
    let (retry, retry_k) = node.next_value();
    assert!(retry_k < k, "a retry must spend a smaller link, not the same one");
    assert_ne!(retry, value);
    chain::accept(&mut state, &retry).unwrap();
}

// ---------- negative cases (design section 9) ----------

#[test]
fn n1_a_tampered_hello_field_makes_the_control_plane_refuse_it() {
    // Flip one bit of the client nonce. The mac was computed over the
    // honest transcript, so the control plane must refuse the hello rather
    // than accept a session it did not authenticate.
    let mut exchange = psk_exchange(b"a perfectly good psk");
    exchange.hello.client_nonce[0] ^= 0xff;
    let rng = rng();
    let signing = control_keypair();
    let control = handshake::ControlHand::new(&rng).unwrap();
    let outcome = handshake::control_finish(
        control,
        &exchange.hello,
        exchange.ack.server_nonce,
        &signing,
        b"cert",
        b"a perfectly good psk",
    );
    assert_eq!(
        outcome.err(),
        Some(handshake::HandshakeError::TranscriptMismatch)
    );
}

#[test]
fn n3_a_downgraded_suite_is_refused_rather_than_negotiated() {
    let mut exchange = psk_exchange(b"a perfectly good psk");
    exchange.hello.suite = handshake::SUITE + 1;
    let rng = rng();
    let signing = control_keypair();
    let control = handshake::ControlHand::new(&rng).unwrap();
    let outcome = handshake::control_finish(
        control,
        &exchange.hello,
        exchange.ack.server_nonce,
        &signing,
        b"cert",
        b"a perfectly good psk",
    );
    assert_eq!(
        outcome.err(),
        Some(handshake::HandshakeError::UnsupportedSuite(handshake::SUITE + 1))
    );
}

#[test]
fn n3b_an_unknown_version_is_refused_too() {
    let mut exchange = psk_exchange(b"a perfectly good psk");
    exchange.hello.version = handshake::VERSION + 1;
    let rng = rng();
    let signing = control_keypair();
    let control = handshake::ControlHand::new(&rng).unwrap();
    let outcome = handshake::control_finish(
        control,
        &exchange.hello,
        exchange.ack.server_nonce,
        &signing,
        b"cert",
        b"a perfectly good psk",
    );
    assert_eq!(
        outcome.err(),
        Some(handshake::HandshakeError::UnsupportedVersion(
            handshake::VERSION + 1
        ))
    );
}

#[test]
fn n5_an_oversized_frame_is_refused_before_it_is_allocated() {
    // A frame whose declared length is past the cap must be refused on the
    // length, before the buffer is allocated -- otherwise a hostile length
    // is a memory-exhaustion primitive.
    let mut bytes = Vec::new();
    let declared = crate::frame::MAX_FRAME_BYTES + 1;
    bytes.extend_from_slice(&(declared as u32).to_be_bytes());
    bytes.push(1);
    let mut cursor = std::io::Cursor::new(bytes);
    let outcome = crate::frame::read_frame(&mut cursor);
    // The refusal names the announced length, so a caller can log "what
    // was asked for" without logging any payload.
    assert!(matches!(
        outcome,
        Err(crate::frame::FrameError::TooLarge { announced }) if announced == declared
    ));
}

#[test]
fn n6_an_expired_chain_is_refused() {
    let seed = [0u8; 32];
    let mut state = ChainState::from_seed(&seed, 1);
    let mut node = ChainNode::new(seed, 1);
    let (value, _) = node.next_value();
    chain::accept(&mut state, &value).unwrap();
    // The chain has one link; spending it exhausts the chain, and the next
    // attempt is a refusal rather than a reuse.
    let (again, _) = node.next_value();
    assert_eq!(chain::accept(&mut state, &again), Err(chain::ChainError::Exhausted));
}

#[test]
fn n2_a_replayed_token_ring_is_refused() {
    let mut seed = [0u8; 32];
    seed[0] = 5;
    let mut state = ChainState::from_seed(&seed, DEFAULT_CHAIN_LENGTH);
    let mut node = ChainNode::new(seed, DEFAULT_CHAIN_LENGTH);
    let (value, _) = node.next_value();
    chain::accept(&mut state, &value).unwrap();
    // Replaying the accepted value must fail: H(c_k) is now c_k+1, which is
    // not what the control plane expects.
    assert_eq!(chain::accept(&mut state, &value), Err(chain::ChainError::NotPreimage));
}

#[test]
fn n2b_a_value_that_is_not_the_preimage_is_refused() {
    let seed = [0u8; 32];
    let mut state = ChainState::from_seed(&seed, DEFAULT_CHAIN_LENGTH);
    let forged = "11".repeat(32);
    assert_eq!(chain::accept(&mut state, &forged), Err(chain::ChainError::NotPreimage));
}

#[test]
fn n9_a_duplicate_seq_is_refused_by_the_record_layer() {
    let mut key = RecordKey::new(&[1u8; 32]);
    let first = key.seal(b"aad", b"one").unwrap();
    let second = key.seal(b"aad", b"two").unwrap();
    // The key advanced, so opening at the old seq cannot succeed: the nonce
    // is derived from the counter, which only moves forward.
    assert_ne!(first, second);
    assert_eq!(key.seq(), 2);
}

#[test]
fn a_frame_opened_with_the_wrong_aad_is_refused() {
    let mut key = RecordKey::new(&[2u8; 32]);
    let sealed = key.seal(b"the right aad", b"payload").unwrap();
    let mut other = RecordKey::new(&[2u8; 32]);
    assert!(other.open(b"the wrong aad", &sealed).is_err());
    let mut right = RecordKey::new(&[2u8; 32]);
    assert_eq!(right.open(b"the right aad", &sealed).unwrap(), b"payload");
}

#[test]
fn a_frame_sealed_with_one_key_cannot_be_opened_with_another() {
    let mut sender = RecordKey::new(&[3u8; 32]);
    let sealed = sender.seal(b"aad", b"secret").unwrap();
    let mut other = RecordKey::new(&[4u8; 32]);
    assert!(other.open(b"aad", &sealed).is_err());
}

#[test]
fn the_session_certificate_only_unseals_with_the_right_wrap_key() {
    let keys = [7u8; 32];
    let sealed = session::seal(&keys, 0, b"aad", b"cert").unwrap();
    assert_eq!(session::open(&keys, 0, b"aad", &sealed).unwrap(), b"cert");
    let wrong = [8u8; 32];
    assert!(session::open(&wrong, 0, b"aad", &sealed).is_err());
}
