//! One frame on the wire.
//!
//! ```text
//! frame = u32 length (big-endian) | u8 type | payload
//! ```
//!
//! `length` counts the type byte plus the payload, so a frame with an empty
//! payload has `length == 1`. The payload is JSON; the type says how to read it.
//!
//! The upper bound is not decoration: a peer that can make us allocate
//! without limit is a peer that can end the process. `MAX_FRAME_BYTES` is
//! checked *before* any allocation happens, so an oversized frame is
//! refused from its own length field without reading the body.

use std::fmt;
use std::io::{Read, Write};

/// Largest payload the codec will accept, in bytes, type byte included.
///
/// 1 MiB is the design figure (node-link design section 4). A frame claiming
/// more than this is refused from its length field, before the body is read.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Frame types. The values are the wire numbers and must not be renumbered:
/// a peer running a newer build speaks the same numbers.
///
/// Only `Request` and `Response` carry traffic in the L1 (plaintext) stage.
/// The rest exist so the handshake (L2) and event streaming (L2/L4) can
/// arrive without a format change -- a frame type that has to be inserted
/// later is a format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Opens a session (L2). Not emitted by L1.
    Hello = 1,
    /// Answers a hello (L2). Not emitted by L1.
    HelloAck = 2,
    /// One JSON-RPC request. L1 traffic.
    Request = 3,
    /// The result or error for a request. L1 traffic.
    Response = 4,
    /// Server-initiated notification (L2/L4). Not emitted by L1.
    Event = 5,
    /// Re-keys the session (L2). Not emitted by L1.
    Rekey = 6,
    /// Ends the session, with a reason. Not emitted by L1.
    Close = 7,
}

impl FrameType {
    /// The wire number, or `None` for a number this build does not know.
    ///
    /// Unknown numbers are refused rather than ignored: silently skipping a
    /// frame the peer considered meaningful desynchronises both sides, and
    /// the next length field would then be read from the middle of a payload.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hello),
            2 => Some(Self::HelloAck),
            3 => Some(Self::Request),
            4 => Some(Self::Response),
            5 => Some(Self::Event),
            6 => Some(Self::Rekey),
            7 => Some(Self::Close),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// True for the two types that carry JSON-RPC in the L1 stage.
    pub fn is_rpc(self) -> bool {
        matches!(self, Self::Request | Self::Response)
    }

    /// True for the types that travel inside an established session, so
    /// their payloads are sealed with the session keys.
    ///
    /// Hello and HelloAck are excluded on purpose: the design is explicit
    /// that they are not encrypted, because the encryption is what they
    /// establish.
    ///
    /// Close is the interesting case, and the honest answer is that it
    /// depends on which side of the handshake it is sent from. Before a
    /// handshake there are no keys, so a close is necessarily plaintext --
    /// that is how the L1 path refuses a connection. After one, design
    /// section 4 says every frame is sealed, and an unsealed close would be
    /// injectable: an on-path attacker with no key material could send
    /// close{already-connected} and force a disconnect.
    ///
    /// So this predicate answers only "is this type ever sealed", and the
    /// session layer decides per-frame. Anything that wants "is this frame
    /// sealed" must ask the session, not the type.
    pub fn carries_session_payload(self) -> bool {
        matches!(
            self,
            Self::Request | Self::Response | Self::Event | Self::Rekey | Self::Close
        )
    }
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Hello => "hello",
            Self::HelloAck => "hello-ack",
            Self::Request => "request",
            Self::Response => "response",
            Self::Event => "event",
            Self::Rekey => "rekey",
            Self::Close => "close",
        };
        write!(f, "{name}")
    }
}

/// Why a frame could not be read or written.
///
/// The variants are deliberately coarse: a caller needs to know whether to
/// close the connection and whether the peer might be hostile, not which
/// byte offended. `Debug` is derived so a test can assert on the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The length field announced more than `MAX_FRAME_BYTES`.
    TooLarge { announced: usize },
    /// The stream ended inside a frame: mid-header, or the body was short
    /// of what the length field promised.
    Truncated { expected: usize, got: usize },
    /// A frame type number this build does not know.
    UnknownType(u8),
    /// The payload was not valid JSON.
    BadPayload(String),
    /// The stream failed underneath us.
    Io(String),
    /// Nothing arrived within the read timeout the caller set.
    ///
    /// A distinct variant rather than a string inside `Io`, because it is
    /// not a failure: it is the wake-up that lets a quiet session rotate its
    /// keys. A caller checking for it should not have to match on a
    /// formatted message.
    TimedOut,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { announced } => {
                write!(f, "frame of {announced} bytes exceeds the {MAX_FRAME_BYTES} byte limit")
            }
            Self::Truncated { expected, got } => {
                write!(f, "frame ended early: {expected} bytes expected, {got} read")
            }
            Self::UnknownType(value) => write!(f, "unknown frame type {value}"),
            Self::BadPayload(detail) => write!(f, "frame payload is not valid JSON: {detail}"),
            Self::Io(detail) => write!(f, "frame transport error: {detail}"),
            Self::TimedOut => write!(f, "no frame arrived within the read timeout"),
        }
    }
}

impl std::error::Error for FrameError {}

/// One decoded frame. `payload` is the raw JSON bytes, still unparsed:
/// turning it into a request or a response is the caller's business, and
/// keeping it here means this crate never grows a dependency on the RPC
/// envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    /// A frame carrying `value` as its JSON payload.
    pub fn json(frame_type: FrameType, value: &serde_json::Value) -> Result<Self, FrameError> {
        let payload = serde_json::to_vec(value).map_err(|e| FrameError::BadPayload(e.to_string()))?;
        Ok(Self { frame_type, payload })
    }

    /// The payload parsed as JSON. Fails with `BadPayload` if it is not JSON.
    pub fn value(&self) -> Result<serde_json::Value, FrameError> {
        serde_json::from_slice(&self.payload).map_err(|e| FrameError::BadPayload(e.to_string()))
    }

    /// Total wire size: 4 length bytes, the type byte, and the payload.
    pub fn wire_len(&self) -> usize {
        4 + 1 + self.payload.len()
    }
}

/// Write one frame. The length prefix is computed here so a caller cannot
/// desynchronise it from the body by accident.
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<(), FrameError> {
    let body_len = 1 + frame.payload.len();
    if body_len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { announced: body_len });
    }
    let length = u32::try_from(body_len).map_err(|_| FrameError::TooLarge { announced: body_len })?;
    writer.write_all(&length.to_be_bytes()).map_err(io)?;
    writer.write_all(&[frame.frame_type.as_u8()]).map_err(io)?;
    writer.write_all(&frame.payload).map_err(io)?;
    writer.flush().map_err(io)
}

/// Read exactly one frame, or explain why the stream cannot produce one.
///
/// A read that returns `Ok(None)` means a clean end at a frame boundary:
/// zero bytes where a length field was due. That is a peer that closed
/// politely, not an error, and the caller decides whether it is acceptable.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut length_bytes = [0u8; 4];
    let read = read_full(reader, &mut length_bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if read < 4 {
        return Err(FrameError::Truncated { expected: 4, got: read });
    }
    let announced = u32::from_be_bytes(length_bytes) as usize;
    if announced > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { announced });
    }
    // `announced` includes the type byte, so a well-formed frame is at least
    // one byte long. A zero here would leave the type byte unread and the
    // next frame length field read from the payload.
    if announced == 0 {
        return Err(FrameError::Truncated { expected: 1, got: 0 });
    }
    let mut body = vec![0u8; announced];
    let got = read_full(reader, &mut body)?;
    if got < announced {
        return Err(FrameError::Truncated { expected: announced, got });
    }
    let type_byte = body[0];
    let frame_type = FrameType::from_u8(type_byte).ok_or(FrameError::UnknownType(type_byte))?;
    Ok(Some(Frame { frame_type, payload: body[1..].to_vec() }))
}

/// Fill `buffer` completely unless the stream ends first. Returns how many
/// bytes were read, which is the only way to tell a clean close from a torn
/// frame without a second syscall.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize, FrameError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A timeout is its own variant, not a message: it is the wake-up
            // a caller rotates on, and matching on a formatted string is how
            // that check breaks silently.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(FrameError::TimedOut);
            }
            Err(e) => return Err(io(e)),
        }
    }
    Ok(filled)
}

fn io(error: std::io::Error) -> FrameError {
    FrameError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a frame through bytes and back. Every other test in this
    /// module is a variation on "what if the peer is wrong", so the happy
    /// path is pinned first and separately.
    #[test]
    fn a_frame_survives_a_round_trip() {
        let payload = serde_json::json!({ "method": "node_heartbeat", "token": "abc" });
        let frame = Frame::json(FrameType::Request, &payload).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let back = read_frame(&mut cursor).unwrap().expect("a frame was written");
        assert_eq!(back, frame);
        assert_eq!(back.value().unwrap(), payload);
    }

    /// The length prefix counts the type byte. If it did not, a reader would
    /// consume one byte too few and the next frame would start mid-payload --
    /// a bug that only shows up on the second frame of a connection.
    #[test]
    fn the_length_prefix_covers_the_type_byte() {
        let frame = Frame { frame_type: FrameType::Request, payload: b"{}".to_vec() };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        assert_eq!(bytes.len(), 5 + 2, "4 length + 1 type + 2 payload");
        assert_eq!(u32::from_be_bytes(bytes[0..4].try_into().unwrap()), 3);
    }

    /// Several frames back to back must not blur together. This is the case
    /// a single-frame round-trip test cannot see.
    #[test]
    fn consecutive_frames_stay_separate() {
        let first = Frame::json(FrameType::Request, &serde_json::json!({ "n": 1 })).unwrap();
        let second = Frame::json(FrameType::Response, &serde_json::json!({ "n": 2 })).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &first).unwrap();
        write_frame(&mut bytes, &second).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), first);
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), second);
        assert!(read_frame(&mut cursor).unwrap().is_none(), "then the stream ends");
    }

    /// A frame that announces more than the limit is refused from its length
    /// field. The body is never read, so a hostile peer cannot make us
    /// allocate first and fail afterwards.
    #[test]
    fn an_oversized_length_is_refused_before_the_body_is_read() {
        let announced = (MAX_FRAME_BYTES + 1) as u32;
        let mut bytes = announced.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[3u8]);
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert_eq!(error, FrameError::TooLarge { announced: MAX_FRAME_BYTES + 1 });
        // The type byte was never consumed: the cursor is still right after
        // the length field, which is what lets a caller close and forget.
        assert_eq!(cursor.position(), 4);
    }

    /// A stream that ends mid-header is a torn frame, not a clean close.
    #[test]
    fn a_stream_ending_mid_header_is_truncated() {
        let mut cursor = std::io::Cursor::new(vec![0u8, 0u8]);
        let error = read_frame(&mut cursor).unwrap_err();
        assert_eq!(error, FrameError::Truncated { expected: 4, got: 2 });
    }

    /// A body shorter than its own length field promised. The classic
    /// desynchronisation: without this check the next read would treat the
    /// remainder as a length field.
    #[test]
    fn a_body_shorter_than_its_length_is_truncated() {
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[3u8, b'{', b'}']);
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert_eq!(error, FrameError::Truncated { expected: 10, got: 3 });
    }

    /// An empty stream is a clean close at a frame boundary, which is not an
    /// error -- the caller decides whether it is acceptable.
    #[test]
    fn an_empty_stream_reads_as_a_clean_close() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    /// A zero length would leave the type byte unread. Refusing it keeps the
    /// stream in a known state instead of silently mis-framing everything
    /// that follows.
    #[test]
    fn a_zero_length_is_refused() {
        let mut bytes = 0u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[3u8, b'{', b'}']);
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert_eq!(error, FrameError::Truncated { expected: 1, got: 0 });
    }

    /// An unknown type number is refused, not skipped. Skipping it would
    /// desynchronise both sides: the peer believes it sent something
    /// meaningful and the next length field would be read from payload bytes.
    #[test]
    fn an_unknown_type_number_is_refused() {
        let mut bytes = 2u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[99u8, b'{']);
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert_eq!(error, FrameError::UnknownType(99));
    }

    /// The type numbers are the wire contract. Renumbering one silently
    /// breaks a peer on a different build, so they are pinned here.
    #[test]
    fn the_type_numbers_are_the_wire_contract() {
        assert_eq!(FrameType::Hello.as_u8(), 1);
        assert_eq!(FrameType::HelloAck.as_u8(), 2);
        assert_eq!(FrameType::Request.as_u8(), 3);
        assert_eq!(FrameType::Response.as_u8(), 4);
        assert_eq!(FrameType::Event.as_u8(), 5);
        assert_eq!(FrameType::Rekey.as_u8(), 6);
        assert_eq!(FrameType::Close.as_u8(), 7);
        for value in 1u8..=7 {
            assert!(FrameType::from_u8(value).is_some(), "type {value} must round-trip");
        }
        assert!(FrameType::from_u8(0).is_none());
        assert!(FrameType::from_u8(8).is_none());
    }

    /// L1 speaks only request and response. The other five are declared for
    /// L2/L4 and must not be produced yet -- this test fails if someone
    /// starts emitting them without the handshake that goes with them.
    #[test]
    fn only_request_and_response_carry_rpc_in_l1() {
        assert!(FrameType::Request.is_rpc());
        assert!(FrameType::Response.is_rpc());
        for other in [FrameType::Hello, FrameType::HelloAck, FrameType::Event, FrameType::Rekey, FrameType::Close] {
            assert!(!other.is_rpc(), "{other} is not L1 traffic");
        }
    }

    /// A payload that is not JSON is refused at the value boundary, not
    /// silently passed through as bytes.
    #[test]
    fn a_non_json_payload_is_refused() {
        let frame = Frame { frame_type: FrameType::Request, payload: b"not json".to_vec() };
        assert!(frame.value().is_err());
    }

    /// `wire_len` is what a caller budgets for. It must match what was
    /// actually written, or a rate limit built on it would be wrong.
    #[test]
    fn wire_len_matches_what_is_written() {
        let frame = Frame::json(FrameType::Response, &serde_json::json!({ "ok": true })).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        assert_eq!(frame.wire_len(), bytes.len());
    }

    /// Writing a frame larger than the limit fails at the writer, so a
    /// caller cannot put a partial frame on the wire and then discover the
    /// problem.
    #[test]
    fn writing_an_oversized_payload_fails_before_any_byte_is_written() {
        let frame = Frame { frame_type: FrameType::Request, payload: vec![0u8; MAX_FRAME_BYTES] };
        let mut bytes = Vec::new();
        let error = write_frame(&mut bytes, &frame).unwrap_err();
        assert_eq!(error, FrameError::TooLarge { announced: MAX_FRAME_BYTES + 1 });
        assert!(bytes.is_empty(), "nothing was written");
    }

    /// Error messages name the limit, because the failure mode they guard
    /// against (a peer allocating our memory) is not obvious from the
    /// variant alone.
    #[test]
    fn error_messages_name_the_limit_and_the_shortfall() {
        let too_large = FrameError::TooLarge { announced: 2048 }.to_string();
        assert!(too_large.contains("2048"), "{too_large}");
        assert!(too_large.contains(&MAX_FRAME_BYTES.to_string()), "{too_large}");
        let truncated = FrameError::Truncated { expected: 10, got: 3 }.to_string();
        assert!(truncated.contains("10") && truncated.contains("3"), "{truncated}");
    }
}
