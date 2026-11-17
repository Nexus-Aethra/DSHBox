//! The handshake: bootstrap secret to session keys, then the secret is
//! dropped.
//!
//! This is a pure state machine, deliberately. It takes the local random
//! material and the peer frame as input and returns either the session keys
//! or an error -- no I/O, no clock, no storage. That is what lets the
//! security properties be pinned by test vectors (design section 9): a
//! tampered field, a replayed token ring, a downgraded suite and a stale
//! chain are all inputs to a function, so each one is a test case rather
//! than an integration scenario.
//!
//! The transcript bindings are the part worth getting right:
//!
//! - t1 covers only the client fields, so the node can compute it before
//!   the server has answered.
//! - t2 folds serverNonce and ephB in on top of t1. Both sides bind them,
//!   so substituting either side nonce or public key breaks the session
//!   key or fails to unseal the certificate.
//! - controlSig is over t2 and is required in every mode, because the
//!   control plane identity is proven by a signature the node verifies
//!   against a fingerprint it obtained offline. That is what makes the
//!   first connection mutually authenticated.
//!
//! What is NOT here: the frame codec (frame.rs), the token chain
//! (chain.rs), and the record layer (session.rs). The handshake produces
//! keys; it does not use them.

use ring::agreement;
use ring::rand::SecureRandom;
use ring::signature;
use serde::{Deserialize, Serialize};

use crate::kdf;

/// The protocol version this crate speaks. A peer offering any other value
/// is refused rather than negotiated down with.
pub const VERSION: u16 = 1;

/// The cipher suite. 1 = X25519 + HKDF-SHA256 + ChaCha20-Poly1305.
pub const SUITE: u16 = 1;

/// The three ways a node can authenticate itself.
///
/// Psk and Token both carry a bootstrap secret; NodeKey does not, because
/// a node that already has a registered key reconnects with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    Psk,
    Token,
    NodeKey,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Psk => "psk",
            AuthMode::Token => "token",
            AuthMode::NodeKey => "node-key",
        }
    }
}

/// The node side of a hello: who it is, what it speaks, and the ephemeral
/// public key plus the authentication for this exchange.
#[derive(Clone, Debug)]
pub struct Hello {
    pub node_id: String,
    pub mode: AuthMode,
    /// The version the node speaks. Anything but VERSION is refused.
    pub version: u16,
    /// The suite the node offers. Anything but SUITE is refused.
    pub suite: u16,
    pub client_nonce: [u8; 16],
    /// The node ephemeral X25519 public key.
    pub eph_a: [u8; 32],
    /// psk/token: HMAC over t1 with the bootstrap key.
    pub mac: Option<[u8; 32]>,
    /// node-key: the node long-term public key.
    pub node_pubkey: Option<Vec<u8>>,
    /// node-key: an Ed25519 signature over t1.
    pub node_sig: Option<Vec<u8>>,
}

/// The control plane reply, and everything the node needs to finish.
#[derive(Clone, Debug)]
pub struct HelloAck {
    pub server_nonce: [u8; 16],
    pub eph_b: [u8; 32],
    /// Ed25519 over t2 with the control plane key. Required in every mode.
    pub control_sig: Vec<u8>,
    /// The session certificate, sealed. Its plaintext is opaque here.
    pub sealed_cert: Vec<u8>,
}

/// The session keys, one per direction, and the transcript they came from.
///
/// The transcript is kept so the record layer can use it as additional
/// authenticated data on the first frames.
#[derive(Clone)]
pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub transcript: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keys must never reach a log line, even a debug one.
        f.debug_struct("SessionKeys")
            .field("transcript", &hex(&self.transcript))
            .finish_non_exhaustive()
    }
}

/// Every way a handshake can fail.
///
/// The variants are the reasons the connection is refused, not a single
/// opaque error, because the security tests assert on them: a tampered
/// field must fail as TranscriptMismatch, a downgraded suite as
/// UnsupportedSuite, and so on. A caller that collapses them into one string
/// cannot write those tests.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("unsupported cipher suite {0}")]
    UnsupportedSuite(u16),
    #[error("the transcript does not match the peer mac")]
    TranscriptMismatch,
    #[error("the control plane signature is missing or wrong")]
    ControlSignature,
    #[error("the node signature is missing or wrong")]
    NodeSignature,
    #[error("the session certificate could not be unsealed")]
    SealMismatch,
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------- transcript ----------

/// t1: the client half of the transcript.
///
/// Everything here comes from the hello, so the node can compute it before
/// it has heard anything back. The mode string is included so a hello
/// cannot be replayed against a different mode.
pub fn transcript_client(hello: &Hello) -> [u8; 32] {
    let mut input = Vec::new();
    input.extend_from_slice(b"node-link-hello-v1");
    input.extend_from_slice(&VERSION.to_be_bytes());
    input.extend_from_slice(&SUITE.to_be_bytes());
    input.extend_from_slice(hello.node_id.as_bytes());
    input.extend_from_slice(hello.mode.as_str().as_bytes());
    input.extend_from_slice(&hello.client_nonce);
    input.extend_from_slice(&hello.eph_a);
    // The node long-term key is bound in, so a signature over t1 is a
    // signature over *this key* and not one that can be re-attached to
    // another key. Without this, node-key mode verifies a signature
    // without authenticating anyone.
    if let Some(pubkey) = &hello.node_pubkey {
        input.extend_from_slice(&(pubkey.len() as u32).to_be_bytes());
        input.extend_from_slice(pubkey);
    }
    ring::digest::digest(&ring::digest::SHA256, &input)
        .as_ref()
        .try_into()
        .expect("sha256 is 32 bytes")
}

/// t2: the whole transcript, with the server half folded in.
///
/// Binding serverNonce and ephB here is what stops a man in the middle
/// from substituting its own ephemeral key: the session keys derive from
/// t2, so a substituted ephB yields keys neither side can use.
pub fn transcript_full(t1: &[u8; 32], server_nonce: &[u8; 16], eph_b: &[u8; 32]) -> [u8; 32] {
    let mut input = Vec::with_capacity(t1.len() + 48);
    input.extend_from_slice(b"node-link-transcript-v1");
    input.extend_from_slice(t1);
    input.extend_from_slice(server_nonce);
    input.extend_from_slice(eph_b);
    ring::digest::digest(&ring::digest::SHA256, &input)
        .as_ref()
        .try_into()
        .expect("sha256 is 32 bytes")
}

// ---------- node side ----------

/// What the node needs to answer a hello: its own ephemeral keypair, its
/// authentication material, and the control plane fingerprint it obtained
/// offline with the bootstrap material.
pub struct NodeHand {
    eph_private: agreement::EphemeralPrivateKey,
    /// The control plane Ed25519 public key, delivered out of band.
    control_pubkey: Vec<u8>,
}

impl NodeHand {
    pub fn new(rng: &dyn SecureRandom, control_pubkey: Vec<u8>) -> Result<Self, ring::error::Unspecified> {
        let eph_private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng)?;
        Ok(Self {
            eph_private,
            control_pubkey,
        })
    }

    pub fn public_key(&self) -> Result<[u8; 32], ring::error::Unspecified> {
        let raw = self.eph_private.compute_public_key()?;
        raw.as_ref()
            .try_into()
            .map_err(|_| ring::error::Unspecified)
    }

    /// Agree with the peer public key, consuming this hand.
    ///
    /// The ECDH output is what the session keys and the wrap key both
    /// derive from, so a caller that needs the wrap key (to unseal the
    /// certificate) needs this too. The hand is consumed, exactly as in
    /// node_finish: the ephemeral private key is destroyed by the act of
    /// using it, which is the forward secrecy the design asks for.
    pub fn agree_with(
        self,
        peer_public_key: &[u8; 32],
    ) -> Result<[u8; 32], HandshakeError> {
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, peer_public_key);
        agreement::agree_ephemeral(self.eph_private, &peer, |material| {
            let mut out = [0u8; 32];
            out.copy_from_slice(&material[..32]);
            out
        })
        .map_err(|_| HandshakeError::SealMismatch)
    }
}

/// The node side: verify the control plane and derive the session keys.
///
/// The order matters. The control signature is checked *before* the
/// certificate is unsealed, so a wrong control plane is refused before any
/// of its sealed material is trusted.
pub fn node_finish(
    hand: NodeHand,
    hello: &Hello,
    ack: &HelloAck,
    // Not needed to derive the session keys: they come from the ECDH alone.
    // It is a parameter so the shape mirrors node_unseal.
    _bootstrap: &[u8],
) -> Result<SessionKeys, HandshakeError> {
    if ack.control_sig.is_empty() {
        return Err(HandshakeError::ControlSignature);
    }
    let t1 = transcript_client(hello);
    let t2 = transcript_full(&t1, &ack.server_nonce, &ack.eph_b);

    // The control plane proves itself over the whole transcript. A missing
    // or wrong signature is a refusal, not a degraded mode.
    let peer = signature::UnparsedPublicKey::new(&signature::ED25519, &hand.control_pubkey);
    peer.verify(&t2, &ack.control_sig)
        .map_err(|_| HandshakeError::ControlSignature)?;

    let ecdh = hand.agree_with(&ack.eph_b)?;

    Ok(SessionKeys {
        c2s: kdf::derive(ecdh.as_ref(), kdf::C2S_LABEL, &t2),
        s2c: kdf::derive(ecdh.as_ref(), kdf::S2C_LABEL, &t2),
        transcript: t2,
    })
}

// ---------- control plane side ----------

/// The control plane half of the exchange.
///
/// `secret` is what the mode supplies: the PSK, or the token-chain value
/// the node presented. It is used to check the node mac and to mix into
/// the wrap key, and it is dropped when this struct is.
pub struct ControlHand {
    eph_private: agreement::EphemeralPrivateKey,
}

impl ControlHand {
    pub fn new(rng: &dyn SecureRandom) -> Result<Self, ring::error::Unspecified> {
        Ok(Self {
            eph_private: agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng)?,
        })
    }

    pub fn public_key(&self) -> Result<[u8; 32], ring::error::Unspecified> {
        let raw = self.eph_private.compute_public_key()?;
        raw.as_ref().try_into().map_err(|_| ring::error::Unspecified)
    }
}

/// The control plane side: verify the node, sign the transcript, derive the
/// session keys, and seal the certificate.
///
/// The node mac is checked first, so an unauthenticated node is refused
/// before the control plane spends a signature on it.
pub fn control_finish(
    hand: ControlHand,
    hello: &Hello,
    server_nonce: [u8; 16],
    signing_key: &signature::Ed25519KeyPair,
    session_cert: &[u8],
    secret: &[u8],
    // The key registered for this nodeId in node-keys, if it has one.
    // NodeKey mode requires it to match hello.node_pubkey exactly: the
    // signature proves the node holds the private key, and this proves it
    // is the key we registered.
    registered_pubkey: Option<&[u8]>,
) -> Result<(SessionKeys, HelloAck), HandshakeError> {
    // No downgrade. A peer offering another version or suite is refused
    // before anything is derived, which is what keeps a future suite from
    // being tricked into an old one.
    if hello.version != VERSION {
        return Err(HandshakeError::UnsupportedVersion(hello.version));
    }
    if hello.suite != SUITE {
        return Err(HandshakeError::UnsupportedSuite(hello.suite));
    }

    let t1 = transcript_client(hello);

    // Node authentication. In psk and token mode this is the HMAC over t1;
    // in node-key mode it is the Ed25519 signature over t1.
    match hello.mode {
        AuthMode::Psk | AuthMode::Token => {
            let mac = hello.mac.ok_or(HandshakeError::TranscriptMismatch)?;
            let expected = kdf::hmac_sha256(&kdf::bootstrap_key(secret), &t1);
            if !constant_time_eq(&mac, &expected) {
                return Err(HandshakeError::TranscriptMismatch);
            }
        }
        AuthMode::NodeKey => {
            let pubkey = hello.node_pubkey.as_ref().ok_or(HandshakeError::NodeSignature)?;
            // The key must be the one registered for this nodeId. Verifying
            // the signature alone would let any peer with any key
            // impersonate any nodeId -- "a signature verified" is not "the
            // right node connected", and the difference is exactly this
            // comparison.
            match registered_pubkey {
                Some(registered) if registered == pubkey.as_slice() => {}
                _ => return Err(HandshakeError::NodeSignature),
            }
            let sig = hello.node_sig.as_ref().ok_or(HandshakeError::NodeSignature)?;
            let peer = signature::UnparsedPublicKey::new(&signature::ED25519, pubkey);
            peer.verify(&t1, sig).map_err(|_| HandshakeError::NodeSignature)?;
        }
    }

    let eph_b = hand.public_key().map_err(|_| HandshakeError::SealMismatch)?;
    let t2 = transcript_full(&t1, &server_nonce, &eph_b);

    let control_sig = signing_key.sign(&t2).as_ref().to_vec();

    // The wrap key mixes the ECDH secret with the bootstrap secret. In psk
    // mode that is an independent proof the control plane holds the PSK
    // (it never travelled); in token mode it is defence in depth only,
    // because the chain value is on the wire.
    let peer_a = agreement::UnparsedPublicKey::new(&agreement::X25519, &hello.eph_a);
    let ecdh = agreement::agree_ephemeral(hand.eph_private, &peer_a, |material| {
        let mut out = [0u8; 32];
        out.copy_from_slice(&material[..32]);
        out
    })
    .map_err(|_| HandshakeError::SealMismatch)?;

    let wrap_key = match hello.mode {
        AuthMode::NodeKey => kdf::derive(ecdh.as_ref(), kdf::WRAP_LABEL, &t2),
        _ => {
            let mut mix = Vec::with_capacity(64);
            mix.extend_from_slice(ecdh.as_ref());
            mix.extend_from_slice(&kdf::bootstrap_key(secret));
            kdf::derive(&mix, kdf::WRAP_LABEL, &t2)
        }
    };

    let sealed_cert = crate::session::seal(&wrap_key, 0, &t2, session_cert)?;

    Ok((
        SessionKeys {
            c2s: kdf::derive(ecdh.as_ref(), kdf::C2S_LABEL, &t2),
            s2c: kdf::derive(ecdh.as_ref(), kdf::S2C_LABEL, &t2),
            transcript: t2,
        },
        HelloAck {
            server_nonce,
            eph_b,
            control_sig,
            sealed_cert,
        },
    ))
}

/// The node side of unsealing: recover the session certificate.
pub fn node_unseal(
    keys: &SessionKeys,
    hello: &Hello,
    ack: &HelloAck,
    // The bootstrap secret is needed here: the wrap key mixes it with the
    // ECDH output, exactly as the control plane computed it. The node
    // recomputes it from its own copy rather than receiving it.
    bootstrap: &[u8],
    eph_private_agreement: &[u8],
) -> Result<Vec<u8>, HandshakeError> {
    let t1 = transcript_client(hello);
    let t2 = keys.transcript;
    debug_assert_eq!(t2, transcript_full(&t1, &ack.server_nonce, &ack.eph_b));

    let wrap_key = match hello.mode {
        AuthMode::NodeKey => kdf::derive(eph_private_agreement, kdf::WRAP_LABEL, &t2),
        _ => {
            let mut mix = Vec::with_capacity(64);
            mix.extend_from_slice(eph_private_agreement);
            mix.extend_from_slice(&kdf::bootstrap_key(bootstrap));
            kdf::derive(&mix, kdf::WRAP_LABEL, &t2)
        }
    };
    crate::session::open(&wrap_key, 0, &t2, &ack.sealed_cert)
}

/// Compare two secrets without an early exit.
///
/// A `==` on a mac leaks how many leading bytes matched, which is enough
/// to forge one byte at a time.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}
