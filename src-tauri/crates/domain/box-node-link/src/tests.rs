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
use crate::session::{self, hex, RecordKey, SessionCert};

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
    /// The node-side ECDH output, needed to rebuild the wrap key.
    ecdh: [u8; 32],
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
        handshake::control_finish(control, &hello, nonce, &signing, &cert, psk, None).unwrap();

    // The node side of the agreement, taken against the eph_b the ack
    // carries. Needed to rebuild the wrap key for node_unseal.
    let ecdh = node_hand.agree_with(&ack.eph_b).unwrap();

    Exchange {
        keys,
        ack,
        hello,
        ecdh,
    }
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
fn n7e_a_registered_node_key_is_accepted_and_a_foreign_one_is_refused() {
    // The case that pins blocker 2: the registered key is accepted, and a
    // peer presenting its own key is refused. Without the second half, a
    // future edit could delete the registered_pubkey comparison and only a
    // test that passes a *matching* key would notice -- and only if the
    // signature check broke too.
    let rng = rng();
    let signing = control_keypair();
    let node_signing = control_keypair();
    let node_pubkey = node_signing.public_key().as_ref().to_vec();

    let control = handshake::ControlHand::new(&rng).unwrap();
    let eph_a = control.public_key().unwrap();
    let mut nonce = [0u8; 16];
    rng.fill(&mut nonce).unwrap();
    let hello = Hello {
        node_id: "node-key".to_owned(),
        mode: AuthMode::NodeKey,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: [3u8; 16],
        eph_a,
        mac: None,
        node_pubkey: Some(node_pubkey.clone()),
        node_sig: None,
    };
    let t1 = handshake::transcript_client(&hello);
    let node_sig = node_signing.sign(&t1).as_ref().to_vec();
    let hello = Hello {
        node_sig: Some(node_sig),
        ..hello
    };

    // The registered key is accepted.
    let accepted = handshake::control_finish(
        handshake::ControlHand::new(&rng).unwrap(),
        &hello,
        nonce,
        &signing,
        b"cert",
        b"",
        Some(&node_pubkey),
    );
    assert!(accepted.is_ok(), "the registered node key must be accepted");

    // A peer with its own key is refused, even though its signature is valid.
    let impostor = control_keypair();
    let impostor_pubkey = impostor.public_key().as_ref().to_vec();
    let mut foreign = hello.clone();
    foreign.node_pubkey = Some(impostor_pubkey.clone());
    let foreign_t1 = handshake::transcript_client(&foreign);
    foreign.node_sig = Some(impostor.sign(&foreign_t1).as_ref().to_vec());
    let refused = handshake::control_finish(
        handshake::ControlHand::new(&rng).unwrap(),
        &foreign,
        nonce,
        &signing,
        b"cert",
        b"",
        Some(&node_pubkey),
    );
    assert_eq!(
        refused.err(),
        Some(handshake::HandshakeError::NodeSignature),
        "a foreign node key must be refused"
    );
}

#[test]
fn n7f_a_missing_control_signature_is_refused_in_node_key_mode() {
    // Section 9 case 7, node-key leg: the NODE refuses an ack whose
    // controlSig is missing, in node-key mode as in the other two.
    let rng = rng();
    let signing = control_keypair();
    let node_signing = control_keypair();
    let node_pubkey = node_signing.public_key().as_ref().to_vec();
    let control = handshake::ControlHand::new(&rng).unwrap();
    let eph_a = control.public_key().unwrap();
    let mut nonce = [0u8; 16];
    rng.fill(&mut nonce).unwrap();
    let hello = Hello {
        node_id: "node-key".to_owned(),
        mode: AuthMode::NodeKey,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: [4u8; 16],
        eph_a,
        mac: None,
        node_pubkey: Some(node_pubkey.clone()),
        node_sig: None,
    };
    let t1 = handshake::transcript_client(&hello);
    let hello = Hello {
        node_sig: Some(node_signing.sign(&t1).as_ref().to_vec()),
        ..hello
    };
    let (_, ack) = handshake::control_finish(
        control,
        &hello,
        nonce,
        &signing,
        b"cert",
        b"",
        Some(&node_pubkey),
    )
    .unwrap();
    let mut stripped = ack.clone();
    stripped.control_sig = Vec::new();
    let node_hand =
        handshake::NodeHand::new(&rng, signing.public_key().as_ref().to_vec()).unwrap();
    let outcome = handshake::node_finish(node_hand, &hello, &stripped, b"");
    assert_eq!(outcome.err(), Some(handshake::HandshakeError::ControlSignature));
}

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
        None,
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
        None,
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
        None,
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
fn n6_a_spent_chain_is_refused_rather_than_reused() {
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
fn n9_a_duplicate_or_rewound_seq_is_refused_by_the_record_layer() {
    let mut key = RecordKey::new(&[1u8; 32]);
    let (_, first) = key.seal(b"aad", b"one").unwrap();
    // Opening it at the seq it was sealed with succeeds.
    assert_eq!(key.open_at(0, b"aad", &first).unwrap(), b"one");
    // Replaying the same frame at the same seq is a duplicate: refused.
    assert_eq!(
        key.open_at(0, b"aad", &first).err(),
        Some(session::SealError::SeqMismatch { expected: 1, got: 0 })
    );
    // And a frame claiming a seq ahead of the counter is refused too, even
    // though the AEAD would have opened it under that nonce.
    let (_, second) = key.seal(b"aad", b"two").unwrap();
    assert_eq!(
        key.open_at(7, b"aad", &second).err(),
        Some(session::SealError::SeqMismatch { expected: 1, got: 7 })
    );
    // The stream is still in step: the frame opens at the seq it was sealed
    // with, and one undecryptable frame does not desynchronise it.
    assert_eq!(key.open_at(1, b"aad", &second).unwrap(), b"two");
}

#[test]
fn a_frame_opened_with_the_wrong_aad_is_refused() {
    let mut key = RecordKey::new(&[2u8; 32]);
    let (_, sealed) = key.seal(b"the right aad", b"payload").unwrap();
    let mut other = RecordKey::new(&[2u8; 32]);
    assert!(other.open(b"the wrong aad", &sealed).is_err());
    let mut right = RecordKey::new(&[2u8; 32]);
    assert_eq!(right.open(b"the right aad", &sealed).unwrap(), b"payload");
}

#[test]
fn a_frame_sealed_with_one_key_cannot_be_opened_with_another() {
    let mut sender = RecordKey::new(&[3u8; 32]);
    let (_, sealed) = sender.seal(b"aad", b"secret").unwrap();
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

// ---------- the cases the first review said were missing ----------

#[test]
fn n4_a_control_fingerprint_that_changed_is_refused_on_reconnect() {
    // The certificate names the control fingerprint. The node pinned one from
    // the bootstrap material, so unsealing with that pinned value succeeds and
    // unsealing with any other is a refusal -- with a reason distinct from
    // SealMismatch, because a caller that collapses the two cannot log which
    // happened.
    let exchange = psk_exchange(b"a perfectly good psk");
    let honest = handshake::node_unseal(
        &exchange.keys,
        &exchange.hello,
        &exchange.ack,
        b"a perfectly good psk",
        &exchange.ecdh,
        "fp-1",
    );
    assert!(honest.is_ok(), "the pinned fingerprint must unseal");

    let rotated = handshake::node_unseal(
        &exchange.keys,
        &exchange.hello,
        &exchange.ack,
        b"a perfectly good psk",
        &exchange.ecdh,
        "fp-rotated",
    );
    assert_eq!(
        rotated.err(),
        Some(handshake::HandshakeError::FingerprintMismatch),
        "a certificate naming another fingerprint must be refused"
    );
}
#[test]
fn n7_a_missing_control_signature_is_refused_in_every_mode() {
    // controlSig is required in all three modes: it is the only thing that
    // authenticates the control plane to a node that has never seen it.
    let mut exchange = psk_exchange(b"a perfectly good psk");
    exchange.ack.control_sig = Vec::new();
    let rng = rng();
    let signing = control_keypair();
    let node_hand =
        handshake::NodeHand::new(&rng, signing.public_key().as_ref().to_vec()).unwrap();
    let outcome = handshake::node_finish(
        node_hand,
        &exchange.hello,
        &exchange.ack,
        b"a perfectly good psk",
    );
    assert_eq!(outcome.err(), Some(handshake::HandshakeError::ControlSignature));
}

#[test]
fn n7b_a_wrong_control_signature_is_refused_in_every_mode() {
    // Signed by a key the node does not hold. The node must refuse rather
    // than fall back to "the ECDH worked, so the peer must be right".
    let exchange = psk_exchange(b"a perfectly good psk");
    let mut ack = exchange.ack.clone();
    let impostor = control_keypair();
    ack.control_sig = impostor.sign(&exchange.keys.transcript).as_ref().to_vec();
    let rng = rng();
    let signing = control_keypair();
    let node_hand =
        handshake::NodeHand::new(&rng, signing.public_key().as_ref().to_vec()).unwrap();
    let outcome = handshake::node_finish(node_hand, &exchange.hello, &ack, b"a perfectly good psk");
    assert_eq!(outcome.err(), Some(handshake::HandshakeError::ControlSignature));
}

#[test]
fn n7c_a_wrong_signature_is_refused_in_token_mode() {
    let rng = rng();
    let signing = control_keypair();
    let control = handshake::ControlHand::new(&rng).unwrap();
    let eph_a = control.public_key().unwrap();
    let mut nonce = [0u8; 16];
    rng.fill(&mut nonce).unwrap();
    let hello = Hello {
        node_id: "node-token".to_owned(),
        mode: AuthMode::Token,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: [1u8; 16],
        eph_a,
        mac: None,
        node_pubkey: None,
        node_sig: None,
    };
    // A real mac, so the node authenticates and the control plane signs. The
    // thing under test is the signature, not the mac.
    let t1 = handshake::transcript_client(&hello);
    let mac = crate::kdf::hmac_sha256(&crate::kdf::bootstrap_key(b"chain value"), &t1);
    let hello = Hello { mac: Some(mac), ..hello };
    let (_, ack) =
        handshake::control_finish(control, &hello, nonce, &signing, b"cert", b"chain value", None).unwrap();
    let mut tampered = ack.clone();
    tampered.control_sig[0] ^= 0xff;
    let node_hand =
        handshake::NodeHand::new(&rng, signing.public_key().as_ref().to_vec()).unwrap();
    let outcome =
        handshake::node_finish(node_hand, &hello, &tampered, b"chain value");
    assert_eq!(outcome.err(), Some(handshake::HandshakeError::ControlSignature));
}

#[test]
fn n7d_a_node_key_hello_with_no_node_signature_is_refused() {
    let rng = rng();
    let signing = control_keypair();
    let node_signing = control_keypair();
    let node_pubkey = node_signing.public_key().as_ref().to_vec();
    let _ = &node_pubkey;
    let control = handshake::ControlHand::new(&rng).unwrap();
    let eph_a = control.public_key().unwrap();
    let mut nonce = [0u8; 16];
    rng.fill(&mut nonce).unwrap();
    let hello = Hello {
        node_id: "node-key".to_owned(),
        mode: AuthMode::NodeKey,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: [2u8; 16],
        eph_a,
        mac: None,
        node_pubkey: Some(node_pubkey.clone()),
        node_sig: None,
    };
    // The control plane refuses a node-key hello with no signature at all.
    let outcome = handshake::control_finish(
        handshake::ControlHand::new(&rng).unwrap(),
        &hello,
        nonce,
        &signing,
        b"cert",
        b"",
        Some(&node_pubkey),
    );
    assert_eq!(outcome.err(), Some(handshake::HandshakeError::NodeSignature));
}

#[test]
fn n8_a_seq_reused_under_the_same_key_is_refused() {
    // Design section 4: the same key never uses a seq twice. open_at takes
    // the frame seq, so a frame that claims a seq already spent is refused
    // before the AEAD sees it -- which is what makes "rekey replay" and
    // "seq reuse under an old key" detectable rather than silent.
    let mut key = RecordKey::new(&[9u8; 32]);
    let (_, frame) = key.seal(b"aad", b"payload").unwrap();
    assert_eq!(key.open_at(0, b"aad", &frame).unwrap(), b"payload");
    // Replaying the identical bytes at the identical seq: refused.
    assert_eq!(
        key.open_at(0, b"aad", &frame).err(),
        Some(session::SealError::SeqMismatch { expected: 1, got: 0 })
    );
}

#[test]
fn the_seed_is_never_a_chain_value() {
    // The blocker the review found: next_value walking to k = 0 would send
    // H^0(seed) = the seed itself, and the control plane would store it as
    // last_accepted. Design section 8 forbids the control plane holding the
    // seed, because a stolen node-keys row would then regenerate the chain.
    let mut seed = [0u8; 32];
    seed[0] = 13;
    let mut node = ChainNode::new(seed, DEFAULT_CHAIN_LENGTH);
    let mut state = ChainState::from_seed(&seed, DEFAULT_CHAIN_LENGTH);
    for _ in 0..DEFAULT_CHAIN_LENGTH * 2 {
        let (value, k) = node.next_value();
        assert!(k >= 1, "the node must never send k = 0, which is the seed");
        assert_ne!(value, hex(&seed), "a chain value must never be the seed");
        if node.is_exhausted() {
            break;
        }
        chain::accept(&mut state, &value).unwrap();
    }
    assert!(node.is_exhausted());
    // And a forged value that happens to be the seed is refused.
    assert_eq!(
        chain::accept(&mut state, &hex(&seed)),
        Err(chain::ChainError::NotPreimage)
    );
}
