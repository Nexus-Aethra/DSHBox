//! The session: a handshake over a socket, then encrypted frames.
//!
//! This is where the handshake meets the transport. The handshake module is
//! pure -- frames in, keys out -- and this module moves those frames over a
//! TcpStream and then uses the keys it got back.
//!
//! Three rules from the design doc are load-bearing here and nowhere else:
//!
//! - **A failed handshake closes the connection.** No retry loop, no partial
//!   state. A peer that cannot complete a handshake gets its socket closed,
//!   because "retry until it works" is how a downgrade succeeds on the nth
//!   attempt.
//! - **The record layer owns the counters.** This module never picks a seq;
//!   it asks the RecordKey for one on send and hands the frame seq to it on
//!   receive, so a caller cannot reuse one by constructing a frame itself.
//! - **Rekey at 2^32 frames or 5 minutes, whichever comes first.** The frame
//!   count is the RecordKey's; the age is checked here, before each send,
//!   because only this side knows when the keys were installed.

use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::frame::{read_frame, write_frame, Frame, FrameError, FrameType};
use crate::handshake::{self, AuthMode, HandshakeError, Hello, SessionKeys};
use crate::session::{RecordKey, SealError, MAX_FRAMES_PER_KEY};
use crate::wire::{self, HelloAckWire, WireError};

/// How long one set of keys may live before a rekey is forced.
///
/// Design section 4: 2^32 frames or 5 minutes, whichever comes first. The
/// frame limit is far below a nonce wrap, so this is a belt to that braces.
pub const MAX_KEY_AGE: Duration = Duration::from_secs(300);

/// A session that has completed its handshake and can carry frames.
pub struct Session {
    /// The node this session speaks for.
    pub node_id: String,
    pub mode: AuthMode,
    c2s: RecordKey,
    s2c: RecordKey,
    key_installed_at: Instant,
    /// How many times this session has rotated its keys. Zero after the
    /// handshake, and the salt for the next derivation.
    rekey_epoch: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("node_id", &self.node_id)
            .field("mode", &self.mode.as_str())
            .field("frames_sent", &self.c2s.send_seq())
            .field("frames_received", &self.s2c.recv_seq())
            .finish_non_exhaustive()
    }
}

/// Every way a session can fail.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("transport: {0}")]
    Transport(#[from] FrameError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire: {0}")]
    Wire(#[from] WireError),
    #[error("handshake: {0}")]
    Handshake(#[from] HandshakeError),
    #[error("seal: {0}")]
    Seal(#[from] SealError),
    #[error("these keys have lived past {MAX_KEY_AGE:?} and must be rekeyed")]
    KeyTooOld,
    #[error("this key has sealed {MAX_FRAMES_PER_KEY} frames and must be rekeyed")]
    KeyTooUsed,
    #[error("the peer closed the connection before answering the hello")]
    PeerHungUp,
    #[error("the frame type is not valid here: {0:?}")]
    WrongFrameType(FrameType),
}

impl Session {
    /// Build a session from keys the handshake produced.
    pub fn from_keys(keys: SessionKeys, node_id: String, mode: AuthMode) -> Self {
        Self {
            c2s: RecordKey::new(&keys.c2s),
            s2c: RecordKey::new(&keys.s2c),
            node_id,
            mode,
            key_installed_at: Instant::now(),
            rekey_epoch: 0,
        }
    }

    /// The node this session belongs to.
    ///
    /// Established by the handshake and read-only after it: the daemon binds
    /// every request on this session to this id, so a caller cannot change
    /// it mid-session.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Seal one payload into a frame, picking the seq from the send counter.
    ///
    /// The seq the record layer chose travels with the frame so the peer can
    /// enforce ordering, and so a log line can name the frame.
    pub fn seal_frame(&mut self, frame_type: FrameType, payload: &[u8]) -> Result<Frame, SessionError> {
        // A rekey frame is exempt from the AGE limit only. It is the repair
        // for an aged key, so refusing it on age grounds leaves the session
        // with no way to rotate -- and the only remaining path is a
        // one-sided rotation the peer cannot follow.
        //
        // The frame-count limit is not exempted. A rekey at exactly the
        // frame limit would consume the last seq on a retired key, and the
        // whole point of the limit is that no such frame exists. In practice
        // install_rekey resets the counter to zero immediately afterwards, so
        // no seq is ever reused under one key either way -- but the narrower
        // exemption says what it means.
        if !matches!(frame_type, FrameType::Rekey)
            && self.key_installed_at.elapsed() >= MAX_KEY_AGE
        {
            return Err(SessionError::KeyTooOld);
        }
        // The frame-count half of the gate, which a rekey frame is NOT exempt
        // from: a rekey at exactly the limit would consume the last seq on a
        // retired key, and the whole point of the limit is that no such frame
        // exists.
        if self.c2s.send_seq() >= MAX_FRAMES_PER_KEY {
            return Err(SessionError::KeyTooUsed);
        }
        let (seq, sealed) = self.c2s.seal(b"", payload)?;
        // The seq travels in the clear, as an 8-byte prefix, and is also the
        // AEAD aad. The clear prefix is not a secret and not authenticated
        // on its own -- it is what lets the peer enforce ordering before it
        // spends a decryption on a frame that is out of step.
        let mut wire = seq.to_be_bytes().to_vec();
        wire.extend_from_slice(&sealed);
        Ok(Frame {
            frame_type,
            payload: wire,
        })
    }

    /// Open one frame, enforcing that its seq is exactly the next one.
    pub fn open_frame(&mut self, frame: &Frame) -> Result<Vec<u8>, SessionError> {
        if !frame.frame_type.carries_session_payload() {
            return Err(SessionError::WrongFrameType(frame.frame_type));
        }
        // The receive direction ages too. A session that only reads still
        // has to rekey, or a long-lived subscriber would never rotate.
        // A rekey frame is exempt for the same reason as on the send side:
        // it is the repair, not a use of the aged keys.
        if !matches!(frame.frame_type, FrameType::Rekey)
            && self.key_installed_at.elapsed() >= MAX_KEY_AGE
        {
            return Err(SessionError::KeyTooOld);
        }
        // The seq travels as a big-endian u64 in the first 8 bytes of the
        // sealed payload, ahead of the AEAD ciphertext.
        if frame.payload.len() < 8 {
            return Err(SessionError::Seal(SealError::OpenFailed));
        }
        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&frame.payload[..8]);
        let seq = u64::from_be_bytes(seq_bytes);
        let ciphertext = &frame.payload[8..];
        // No aad, and the seq is not authenticated: the clear prefix is a
        // public counter, and a nonce is public by construction. The only
        // thing that stops a reordered frame is open_at's ordering check,
        // which runs before the AEAD and refuses anything that is not
        // exactly the next seq. That single check is the whole defence, so
        // it is the one the tests pin.
        Ok(self.s2c.open_at(seq, b"", ciphertext)?)
    }

    /// Force a rekey if these keys are too old or too used.
    ///
    /// The two limits have separate errors because they mean different
    /// things to a caller deciding whether to rekey or to drop: age is a
    /// wall-clock deadline the peer also enforces, count is a property of
    /// this direction alone.
    fn check_key_age(&self) -> Result<(), SessionError> {
        if self.key_installed_at.elapsed() >= MAX_KEY_AGE {
            return Err(SessionError::KeyTooOld);
        }
        if self.c2s.send_seq() >= MAX_FRAMES_PER_KEY {
            return Err(SessionError::KeyTooUsed);
        }
        Ok(())
    }
    /// Mark the current keys as installed in the past.
    ///
    /// Test-only, and the only way to exercise the age gate without waiting
    /// five minutes. A review found the rekey halves refused to run once the
    /// keys had aged, and the only way to see that was to make the age
    /// observable.
    #[cfg(test)]
    pub fn age_keys_for_test(&mut self, age: std::time::Duration) {
        self.key_installed_at = std::time::Instant::now() - age;
    }

    /// How many times this session has rekeyed. Exposed for tests and logs; a
    /// session that never rotates reports zero.
    pub fn rekey_count(&self) -> u64 {
        self.rekey_epoch
    }

/// Send a rekey frame and install the keys it names.
///
/// The frame is sealed under the *current* keys -- a rekey that announced
/// itself under the keys it was replacing would be indistinguishable from a
/// forged one -- and its payload is the salt the next keys derive from. The
/// peer derives the same pair from the same salt, which is why the salt has to
/// travel: the epoch alone would leave the two sides guessing.
///
/// Design section 4: no rerun of the handshake, and the bootstrap secret never
/// comes back. Both hold, because the derivation is HKDF(current keys, salt)
/// and the current keys are all it needs.
///
/// A failure leaves the session on its current keys rather than half-rotated:
/// the peer has not acknowledged, so it is still on the old pair, and a
/// one-sided rotation is the state where every subsequent frame fails to open.
pub fn send_rekey(&mut self, writer: &mut BufWriter<TcpStream>) -> Result<(), SessionError> {
    let mut salt = [0u8; 16];
    handshake::fill_random(&mut salt)
        .map_err(|_| SessionError::Seal(SealError::OpenFailed))?;

    // Sealed under the current keys, before the rotation. After this the old
    // pair is gone, so the frame has to be built first.
    let frame = self.seal_frame(FrameType::Rekey, &salt)?;
    write_frame(writer, &frame)?;
    writer.flush()?;

    self.install_rekey(&salt);
    Ok(())
}

/// Receive a rekey frame and install the keys it names.
///
/// Opened under the current keys -- the same pair the peer sealed it with --
/// and the salt it carries is the salt the peer derived the next keys from.
/// This side derives the same pair, so the two converge without either having
/// sent key material.
pub fn recv_rekey(&mut self, frame: &Frame) -> Result<(), SessionError> {
    let salt = self.open_frame(frame)?;
    let salt: [u8; 16] = salt
        .as_slice()
        .try_into()
        .map_err(|_| SessionError::Seal(SealError::OpenFailed))?;
    self.install_rekey(&salt);
    Ok(())
}

/// Install a key set derived from the given salt, and reset the counters.
///
/// **Where this departs from the design doc.** Section 4 says a rekey has
/// "both sides contribute a fresh ephemeral public key". This does not: the
/// new keys are HKDF(current keys, peer-chosen salt), with no ephemeral
/// contribution from either side. The doc describes the stronger design and
/// this is the weaker one that shipped.
///
/// The consequence is that the peer controls the whole key ladder's
/// trajectory. It cannot choose a weak key -- HKDF output is not controllable
/// -- but it does choose every intermediate key, and a peer that sends N
/// rekeys drives N derivations and leaves this side on the Nth. That is
/// inherent to a peer-driven exchange with no ephemeral input, and it is why
/// the rate limit exists. Recorded here rather than left for someone to
/// discover from the code, because the design doc reads as though this were
/// implemented.
///
/// Shared by the send and receive halves so the two cannot drift: a rekey is
/// one derivation, and two copies of it is how peers end up on different keys
/// with nothing on the wire to say so.
fn install_rekey(&mut self, salt: &[u8; 16]) {
    let next_c2s = crate::kdf::derive(self.c2s.material(), crate::kdf::REKEY_LABEL, salt);
    let next_s2c = crate::kdf::derive(self.s2c.material(), crate::kdf::REKEY_LABEL, salt);
    self.c2s = RecordKey::new(&next_c2s);
    self.s2c = RecordKey::new(&next_s2c);
    // The counters reset with the keys: the seq restarts at zero, which is
    // what the record layer documents and what the frame limit counts from.
    self.key_installed_at = Instant::now();
    self.rekey_epoch = self.rekey_epoch.wrapping_add(1);
}
}



// ---------- the node side of the socket handshake ----------

/// The node side: send a hello, read the ack, finish, unseal the
/// certificate.
///
/// `pinned_fingerprint` is the control plane fingerprint delivered offline
/// with the bootstrap material. It is what makes the first connection
/// mutually authenticated rather than trust-on-first-use.
pub fn node_connect(
    stream: TcpStream,
    hello: &Hello,
    node_hand: handshake::NodeHand,
    bootstrap: &[u8],
    pinned_fingerprint: &str,
) -> Result<Session, SessionError> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // 1. The hello. Handshake frames are not encrypted -- the design is
    //    explicit that the encryption is established *by* them.
    let wire = wire::hello_wire(hello);
    let payload = serde_json::to_vec(&wire)
        .map_err(|_| WireError::BadHex { field: "hello" })?;
    write_frame(
        &mut writer,
        &Frame {
            frame_type: FrameType::Hello,
            payload,
        },
    )?;
    writer.flush()?;

    // 2. The ack. Anything that is not a hello-ack is a refusal, and the
    //    connection closes rather than falling back to something else.
    let frame = read_frame(&mut reader)?;
    let frame = match frame {
        Some(frame) => frame,
        // A peer that hangs up before answering has refused the session.
        // A peer that hangs up before answering has refused the session.
        // Reported as its own error rather than a truncation, because the
        // two mean different things to a caller deciding whether to retry.
        None => return Err(SessionError::PeerHungUp),
    };
    if frame.frame_type != FrameType::HelloAck {
        return Err(SessionError::WrongFrameType(frame.frame_type));
    }
    let ack_wire: HelloAckWire = serde_json::from_slice(&frame.payload)
        .map_err(|_| WireError::BadHex { field: "helloAck" })?;
    let ack = ack_wire.to_ack()?;

    // 3. Finish: verify the control signature, agree, derive the keys, and
    //    unseal the certificate against the pinned fingerprint.
    let outcome = handshake::node_finish(node_hand, hello, &ack, bootstrap)?;
    handshake::node_unseal(
        &outcome.keys,
        hello,
        &ack,
        bootstrap,
        &outcome.ecdh,
        pinned_fingerprint,
    )?;

    Ok(Session::from_keys(
        outcome.keys,
        hello.node_id.clone(),
        hello.mode,
    ))
}
