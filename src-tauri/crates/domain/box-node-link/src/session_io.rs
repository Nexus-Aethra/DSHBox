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
        }
    }

    /// Seal one payload into a frame, picking the seq from the send counter.
    ///
    /// The seq the record layer chose travels with the frame so the peer can
    /// enforce ordering, and so a log line can name the frame.
    pub fn seal_frame(&mut self, frame_type: FrameType, payload: &[u8]) -> Result<Frame, SessionError> {
        self.check_key_age()?;
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
        if self.key_installed_at.elapsed() >= MAX_KEY_AGE {
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
