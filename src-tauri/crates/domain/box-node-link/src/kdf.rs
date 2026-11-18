//! Key derivation for the node link handshake.
//!
//! Three derivations, each with its own label, because they are used for
//! different things and a label shared between them would let one
//! substitution be mistaken for another:
//!
//! - `bootstrap`  the node authentication key. HMAC over a transcript
//!   digest, so tampering with any hello field invalidates the mac.
//! - `wrap`       the key that seals the session certificate. It mixes in
//!   the ECDH shared secret *and* the bootstrap secret, which is what makes
//!   it a proof of possession rather than just a key agreement.
//! - `c2s`/`s2c`  the two session keys. Separate labels, so a frame
//!   encrypted for one direction can never be replayed as the other.
//!
//! Every derivation is HKDF-SHA256 over the same construction, differing
//! only in the label and the salt. There is no hand-rolled KDF here: ring
//! provides HKDF, and the design doc (section 9) requires that it stays
//! that way.

use ring::hkdf;
use ring::hmac;

/// The bootstrap label. Deriving the authentication key with it is what
/// makes a PSK and a token-chain value interchangeable at this step: both
/// become "a 32-byte secret" and the mode only decides where it came from.
pub const BOOTSTRAP_LABEL: &[u8] = b"bootstrap";
/// The rekey derivation label.
///
/// Distinct from the handshake labels so a key derived for one purpose can
/// never be mistaken for a key derived for another, even by a caller that
/// passes the same salt.
pub const REKEY_LABEL: &[u8] = b"node-link-rekey-v1";

pub const WRAP_LABEL: &[u8] = b"wrap";
pub const C2S_LABEL: &[u8] = b"c2s";
pub const S2C_LABEL: &[u8] = b"s2c";

/// Derive a 32-byte key from a secret and a label.
///
/// `info` is the binding material: the transcript digest, so a derived key
/// is only valid for the exact exchange that produced it.
pub fn derive(secret: &[u8], label: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, label);
    // 32 bytes is short enough that the extract step always yields exactly
    // one output block, so this cannot fail.
    // Held in its own binding: the Okm borrows the Prk, which borrows the
    // Salt, so chaining them in one expression leaves the salt a temporary.
    let prk = salt.extract(secret);
    let info = [info];
    let out = prk
        .expand(&info, hkdf::HKDF_SHA256)
        .expect("32 bytes of HKDF output");
    let mut key = [0u8; 32];
    out.fill(&mut key).expect("32 bytes of HKDF output");
    key
}

/// The bootstrap authentication key for a mode secret.
pub fn bootstrap_key(secret: &[u8]) -> [u8; 32] {
    derive(secret, BOOTSTRAP_LABEL, b"node-link-bootstrap-v1")
}

/// HMAC-SHA256 over `message` with `key`. Used for the handshake mac and
/// for the token-chain hash links.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&key, message);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}