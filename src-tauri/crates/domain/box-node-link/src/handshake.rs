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
use ring::signature::{self, KeyPair};
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
///
/// PartialEq is derived so a round-trip through the wire form can be asserted
/// on directly, rather than field by field.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// token: the chain link being spent, c_k.
    ///
    /// The control plane holds the chain end and accepts a value only when
    /// H(value) equals it, so this is what makes the link spendable at all.
    /// It is bound into the transcript, which is what stops a link being
    /// lifted off one hello and re-attached to another.
    pub link: Option<[u8; 32]>,
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
    /// token mode: the presented link is not the preimage of the stored
    /// chain end, so it is not the next link in the chain.
    #[error("the presented chain link is not the preimage of the stored chain end")]
    ChainMismatch,
    #[error("the control plane signature is missing or wrong")]
    ControlSignature,
    #[error("the node signature is missing or wrong")]
    NodeSignature,
    #[error("the session certificate could not be unsealed")]
    SealMismatch,
    #[error("the control plane fingerprint is not the one pinned offline")]
    FingerprintMismatch,
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
    // The chain link, for the same reason: a link lifted off one hello and
    // re-attached to another must not produce the same transcript, or the
    // mac that proved possession of the secret still verifies.
    if let Some(link) = &hello.link {
        input.extend_from_slice(link);
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
/// What the node side produces: the session keys, and the ECDH output they
/// came from.
///
/// The ECDH is returned because unsealing the certificate needs it: the wrap
/// key derives from the ECDH mixed with the bootstrap secret. Dropping it
/// inside node_finish would force the caller to redo the agreement, which it
/// cannot -- the ephemeral private key was consumed.
pub struct NodeOutcome {
    pub keys: SessionKeys,
    pub ecdh: [u8; 32],
}

pub fn node_finish(
    hand: NodeHand,
    hello: &Hello,
    ack: &HelloAck,
    // Not needed to derive the session keys: they come from the ECDH alone.
    // It is a parameter so the shape mirrors node_unseal.
    _bootstrap: &[u8],
) -> Result<NodeOutcome, HandshakeError> {
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

    Ok(NodeOutcome {
        keys: SessionKeys {
            c2s: kdf::derive(&ecdh, kdf::C2S_LABEL, &t2),
            s2c: kdf::derive(&ecdh, kdf::S2C_LABEL, &t2),
            transcript: t2,
        },
        ecdh,
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
    signing_key: &ControlKey,
    session_cert: &[u8],
    secret: &[u8],
    // The key registered for this nodeId in node-keys, if it has one.
    // NodeKey mode requires it to match hello.node_pubkey exactly: the
    // signature proves the node holds the private key, and this proves it
    // is the key we registered.
    registered_pubkey: Option<&[u8]>,
) -> Result<(SessionKeys, HelloAck, Option<[u8; 32]>), HandshakeError> {
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

    // The link this handshake spent, for the caller to persist before the
    // reply goes out. Token mode fills it; the others leave it None.
    let mut spent_link: Option<[u8; 32]> = None;

    // Node authentication. In psk and token mode this is the HMAC over t1;
    // in node-key mode it is the Ed25519 signature over t1.
    match hello.mode {
        AuthMode::Psk => {
            let mac = hello.mac.ok_or(HandshakeError::TranscriptMismatch)?;
            let expected = kdf::hmac_sha256(&kdf::bootstrap_key(secret), &t1);
            if !constant_time_eq(&mac, &expected) {
                return Err(HandshakeError::TranscriptMismatch);
            }
        }
        AuthMode::Token => {
            // The link is spent here, and spending it is the whole point.
            //
            // The control plane holds the chain end c_n and accepts a value
            // only when H(value) equals it, so the link the node presents is
            // c_(n-1) and this check is what retires it. The caller must
            // persist the new expectation (the presented value itself) before
            // this returns, which is why the value travels in `spent` rather
            // than being dropped: the next handshake must present its
            // preimage.
            //
            // Authenticating over the chain end instead -- which is what an
            // earlier revision did -- leaves the link unspent, so the same
            // mac is reusable for the life of the row: any peer that once
            // read a hello can mint a fresh one with a new nonce and be
            // handed a session as that node. That is the design's own
            // continuation property defeated without breaking a hash.
            let link = hello.link.ok_or(HandshakeError::ChainMismatch)?;
            let hashed = crate::chain::hash_once(&link);
            // The stored chain end is the only 32-byte value in play, and a
            // length that is not 32 is a malformed row rather than a wrong
            // link.
            let expected: [u8; 32] = secret
                .try_into()
                .map_err(|_| HandshakeError::ChainMismatch)?;
            if !constant_time_eq(&hashed, &expected) {
                return Err(HandshakeError::ChainMismatch);
            }
            // And the mac, over the same link, proves the node holds it.
            let mac = hello.mac.ok_or(HandshakeError::TranscriptMismatch)?;
            let expected = kdf::hmac_sha256(&kdf::bootstrap_key(&link), &t1);
            if !constant_time_eq(&mac, &expected) {
                return Err(HandshakeError::TranscriptMismatch);
            }
            spent_link = Some(link);
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

    // The wrap key mixes the ECDH secret with the secret this mode proved.
    //
    // In psk mode that is an independent proof the control plane holds the
    // PSK, which never travelled. In token mode the mix is over the *link
    // the node just spent*, not over the stored chain end: the node derives
    // the same key from the link it presented, and mixing the chain end
    // here would give the two sides different keys and a certificate that
    // will not unseal. It is defence in depth either way, because the
    // chain value is on the wire.
    let wrap_key = match hello.mode {
        AuthMode::NodeKey => kdf::derive(ecdh.as_ref(), kdf::WRAP_LABEL, &t2),
        AuthMode::Token => {
            let link = hello.link.ok_or(HandshakeError::ChainMismatch)?;
            let mut mix = Vec::with_capacity(64);
            mix.extend_from_slice(ecdh.as_ref());
            mix.extend_from_slice(&kdf::bootstrap_key(&link));
            kdf::derive(&mix, kdf::WRAP_LABEL, &t2)
        }
        AuthMode::Psk => {
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
        spent_link,
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
    // The fingerprint the node pinned from the bootstrap material. The
    // certificate must name this one, or the control plane it is talking to
    // is not the one it enrolled with.
    pinned_fingerprint: &str,
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
    let plaintext = crate::session::open(&wrap_key, 0, &t2, &ack.sealed_cert)?;
    // Change detection: the certificate names a fingerprint, and it has to be
    // the one pinned offline. A rotated control-plane key fails earlier at
    // ControlSignature, so reaching here means the signature verified --
    // which is exactly when a stale fingerprint has to be caught.
    let cert: crate::session::SessionCert =
        serde_json::from_slice(&plaintext).map_err(|_| HandshakeError::SealMismatch)?;
    if cert.control_fingerprint != pinned_fingerprint {
        return Err(HandshakeError::FingerprintMismatch);
    }
    Ok(plaintext)
}

/// Compare two macs without an early exit.
///
/// A `==` on a mac leaks how many leading bytes matched, which is enough to
/// forge one byte at a time.
///
/// This is hand-written because ring 0.17 offers no public alternative:
/// `ring::constant_time::verify_slices_are_equal` is the whole of that module
/// and it is itself deprecated, with ring's own note saying it carries "no
/// promises regarding side channels". The implementation ring uses
/// internally (`bb::verify_slices_are_equal`) is pub(crate) and unreachable
/// from here. So the choice is this loop or nothing.
///
/// The loop is the fixed-size, no-branch shape on purpose: both inputs are
/// 32 bytes by type, so there is no length to check and nothing for an editor
/// to add a branch to. `ring::hint::black_box` on the accumulator keeps the
/// compiler from short-circuiting it.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    std::hint::black_box(diff) == 0
}

// ---------- the control plane long-term key ----------

/// A control-plane Ed25519 key pair, as the parts a caller needs.
///
/// The public key and its fingerprint are what a node pins offline; the
/// PKCS#8 document is what gets stored. Neither is the private key in a form
/// anyone should log, which is why the function returns them separately and
/// the caller decides what to persist.
pub struct ControlKeypair {
    /// Ed25519 public key, hex.
    pub pubkey_hex: String,
    /// PKCS#8 DER document, hex. This is the secret.
    pub seckey_hex: String,
    /// Short fingerprint of the public key.
    pub fingerprint: String,
}

impl std::fmt::Debug for ControlKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlKeypair")
            .field("pubkey_hex", &self.pubkey_hex)
            .field("seckey_hex", &"<redacted>")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Generate a control-plane key pair.
///
/// Lives here rather than in the daemon because the primitive, the encoding
/// and the fingerprint scheme belong with the rest of the crypto: a daemon
/// that generated keys itself would need its own ring dependency and its own
/// idea of how a fingerprint is computed, and the two would drift.
pub fn generate_control_keypair() -> Result<ControlKeypair, HandshakeError> {
    let rng = ring::rand::SystemRandom::new();
    let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| HandshakeError::ControlSignature)?;
    let key = ring::signature::Ed25519KeyPair::from_pkcs8(document.as_ref())
        .map_err(|_| HandshakeError::ControlSignature)?;
    let pubkey_hex = hex(key.public_key().as_ref());
    let fingerprint = crate::node_keys::fingerprint_of(&pubkey_hex);
    Ok(ControlKeypair {
        pubkey_hex,
        seckey_hex: hex(document.as_ref()),
        fingerprint,
    })
}

/// The control-plane signing key.
///
/// An alias so callers outside this crate can name the type without
/// depending on `ring` themselves. The key is a handle over bytes the
/// store owns; nothing here exposes the private material.
pub type ControlKey = ring::signature::Ed25519KeyPair;

/// Rebuild a signing key from a stored PKCS#8 document.
pub fn control_signing_key(seckey_hex: &str) -> Result<ControlKey, HandshakeError> {
    let bytes = hex_decode(seckey_hex).ok_or(HandshakeError::ControlSignature)?;
    ring::signature::Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|_| HandshakeError::ControlSignature)
}


/// Decode hex, or None on a malformed or odd-length input.
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// A fresh random nonce for a hello.
///
/// Lives here rather than in the daemon so that `ring` stays a dependency of
/// the crate that owns the crypto. A daemon that reached for `ring` itself to
/// fill a nonce would need its own copy of the primitive and its own idea of
/// what a nonce is, and the two would drift.
pub fn fresh_nonce() -> Result<[u8; 16], HandshakeError> {
    let mut nonce = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| HandshakeError::ControlSignature)?;
    Ok(nonce)
}

/// Fill a buffer with random bytes.
///
/// Same reason as `fresh_nonce`: the daemon needs entropy and should not need
/// `ring` itself to get it. `SecureRandom` is a trait, so re-exporting the type
/// alone is not enough -- the method would not resolve -- and making every
/// caller import the trait is how a second copy of the primitive arrives.
pub fn fill_random(dest: &mut [u8]) -> Result<(), HandshakeError> {
    ring::rand::SystemRandom::new()
        .fill(dest)
        .map_err(|_| HandshakeError::ControlSignature)
}

