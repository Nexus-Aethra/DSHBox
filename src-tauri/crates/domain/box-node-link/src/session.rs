//! The record layer: AEAD over one direction of a session.
//!
//! One key and one sequence counter per direction, never shared. The nonce
//! is the counter with a fixed prefix, which is what makes a replayed or
//! rewound frame fail: the counter only ever moves forward, and a frame
//! whose seq is not exactly the next one is refused rather than buffered.
//!
//! This is the layer the design doc section 4 calls the only part of a
//! self-built AEAD frame protocol that has to be written by hand, so the
//! rules are enforced here and nowhere else:
//!
//! - the nonce is 96 bits, 4 bytes of zero then the 8-byte seq, so a seq
//!   can never repeat under the same key;
//! - seq starts at zero after the handshake and after every rekey;
//! - a duplicate or backwards seq closes the connection;
//! - a key is retired after 2^32 frames or 5 minutes, whichever comes
//!   first -- the caller checks the age, this module counts the frames.

use ring::aead::{self, LessSafeKey, UnboundKey};
use serde::{Deserialize, Serialize};

use crate::handshake::HandshakeError;

/// The AEAD in use: ChaCha20-Poly1305.
///
/// ring exposes it as CHACHA20_POLY1305. The design doc originally named
/// HChaCha20, which is a different primitive; the suite is this one.
const ALGORITHM: &aead::Algorithm = &aead::CHACHA20_POLY1305;

/// How many frames one key may seal before it must be rotated.
///
/// 2^32 is far below the point where a 96-bit nonce built from a counter
/// could wrap, so this is a belt to that braces.
pub const MAX_FRAMES_PER_KEY: u64 = 1 << 32;

/// Every way a record can fail.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("the sequence counter has reached its limit; rekey first")]
    CounterExhausted,
    #[error("the frame failed to open; it was tampered with, misordered, or from another key")]
    OpenFailed,
    #[error("frame seq {got} is not the expected {expected}; treated as an attack")]
    SeqMismatch { expected: u64, got: u64 },
}

/// One direction of a session.
///
/// The counter is part of the key state, not a parameter, so a caller
/// cannot accidentally reuse a seq by constructing frames itself. It is
/// deliberately not Clone: two clones would share a key and a counter,
/// and the second one would produce the same nonce under the same key as
/// the first -- a nonce reuse, which is fatal for an AEAD.
pub struct RecordKey {
    key: LessSafeKey,
    /// The bytes `key` was built from, kept for the rekey derivation.
    raw: [u8; 32],
    /// How many frames this side has sealed. Drives the nonce, and never
    /// repeats.
    send_seq: u64,
    /// How many frames this side has opened. The next frame must claim
    /// exactly this, which is what makes a replay detectable.
    recv_seq: u64,
}

impl RecordKey {
    /// The raw key bytes, for deriving the next epoch.
    ///
    /// Crate-private for the same reason the field is: a key that can be read
    /// is a key that can be leaked. It exists for `Session::rekey`, which is
    /// the one place the next keys legitimately come from the current ones.
    pub(crate) fn material(&self) -> &[u8; 32] {
        &self.raw
    }

    pub fn new(key_bytes: &[u8; 32]) -> Self {
        let unbound = UnboundKey::new(ALGORITHM, key_bytes).expect("a 32-byte key is valid");
        Self {
            key: LessSafeKey::new(unbound),
            raw: *key_bytes,
            send_seq: 0,
            recv_seq: 0,
        }
    }

    /// How many frames this side has sealed so far.
    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// How many frames this side has opened so far.
    pub fn recv_seq(&self) -> u64 {
        self.recv_seq
    }

    /// Seal `plaintext` and return the ciphertext with its tag appended.
    ///
    /// The seq this frame was sealed with is returned alongside it, because
    /// the counter advances as part of sealing: a caller that wants to open
    /// the frame again (or to log which frame it was) needs to know the seq
    /// rather than infer it.
    pub fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Result<(u64, Vec<u8>), SealError> {
        if self.send_seq >= MAX_FRAMES_PER_KEY {
            return Err(SealError::CounterExhausted);
        }
        let seq = self.send_seq;
        let nonce = nonce_for(seq);
        self.send_seq += 1;
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut in_out)
            .map_err(|_| SealError::OpenFailed)?;
        Ok((seq, in_out))
    }

    /// Open one sealed record, at the seq the frame claims.
    ///
    /// Design section 4: a duplicate or backwards seq is an attack and the
    /// connection is closed, not buffered. So the seq is a parameter and
    /// anything other than exactly the next one is refused *before* the
    /// AEAD runs -- a frame that would open under the right nonce but
    /// claims the wrong seq is still a refusal.
    pub fn open_at(&mut self, seq: u64, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, SealError> {
        // The only frame that may be opened is the next one in sequence:
        // exactly recv_seq. Anything else -- a duplicate of one already
        // opened, or one ahead of the counter -- is refused before the
        // AEAD runs, which is what makes a replay detectable even when the
        // bytes would otherwise open.
        if seq != self.recv_seq {
            return Err(SealError::SeqMismatch {
                expected: self.recv_seq,
                got: seq,
            });
        }
        let nonce = nonce_for(seq);
        let mut in_out = ciphertext.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, aead::Aad::from(aad), &mut in_out)
            .map_err(|_| SealError::OpenFailed)?;
        self.recv_seq = seq + 1;
        Ok(plain.to_vec())
    }

    /// Open the record this key expects next, when the caller has no seq of
    /// its own to check against.
    pub fn open(&mut self, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, SealError> {
        self.open_at(self.recv_seq, aad, ciphertext)
    }
}

/// The 96-bit nonce for a seq: four zero bytes then the counter.
///
/// The fixed prefix keeps the counter unambiguous, and an 8-byte counter
/// cannot wrap inside the frame limit above.
fn nonce_for(seq: u64) -> aead::Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&seq.to_be_bytes());
    aead::Nonce::assume_unique_for_key(bytes)
}

/// Seal with an explicit key and seq, for the handshake certificate.
///
/// The certificate is sealed at seq 0 under the wrap key, which is a
/// different key from either session key, so it cannot collide with a
/// record frame.
pub fn seal(key: &[u8; 32], seq: u64, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, HandshakeError> {
    let unbound = UnboundKey::new(ALGORITHM, key).map_err(|_| HandshakeError::SealMismatch)?;
    let key = LessSafeKey::new(unbound);
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce_for(seq), aead::Aad::from(aad), &mut in_out)
        .map_err(|_| HandshakeError::SealMismatch)?;
    Ok(in_out)
}

/// Open what `seal` produced.
pub fn open(key: &[u8; 32], seq: u64, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, HandshakeError> {
    let unbound = UnboundKey::new(ALGORITHM, key).map_err(|_| HandshakeError::SealMismatch)?;
    let key = LessSafeKey::new(unbound);
    let mut in_out = ciphertext.to_vec();
    let plain = key
        .open_in_place(nonce_for(seq), aead::Aad::from(aad), &mut in_out)
        .map_err(|_| HandshakeError::SealMismatch)?;
    Ok(plain.to_vec())
}

/// The session certificate: what the sealed payload carries.
///
/// Long-lived identity and the change detection the design doc section 5
/// asks for. The fingerprint is the control plane's, so a node that has
/// pinned one fingerprint refuses a certificate naming another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCert {
    pub node_id: String,
    pub control_fingerprint: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

/// Lowercase hex, for logs and test assertions.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
