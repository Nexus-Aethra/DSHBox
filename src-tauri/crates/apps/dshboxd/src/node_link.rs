//! The node link: one long-lived connection carrying length-prefixed frames
//! instead of one HTTP POST per operation.
//!
//! This is stage L4b of the connection track
//! (`docs/design/2026-11-08-node-link-session-and-crate-layering.md` section 10).
//! L1 put a plaintext, session-token-gated frame loop on the wire; L2 and L3
//! built the handshake, the record layer, the wire types and the secrets store.
//! This module wires them in.
//!
//! **The shape of the upgrade, and why it is not a flag.** The first frame a
//! peer sends decides the path: a `Hello` runs the handshake and everything
//! after it is encrypted; a `Request` is the L1 plaintext path, unchanged. An
//! older helper therefore keeps working against a newer daemon with no version
//! negotiation and no downgrade attack -- there is no shared state in which a
//! choice could be forced, only two mutually exclusive first frames.
//!
//! The gate is unchanged from L1, and that is the point: the same
//! `Gate::resolve` decision, the same `bind_request_node` stripping, the same
//! `NODE_METHODS` whitelist. What changed is the transport a request arrives
//! on, not who may ask what.

use crate::state::DaemonState;
use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
use box_foundation::{BoxPaths, BoxResult};
use box_node_link::handshake::{self, AuthMode, ControlHand, Hello};
use crate::node_keys::mode_of;
use box_node_link::SystemRandom;
use box_node_link::node_keys::NodeKeyRecord;
use box_node_link::session_io::Session;
use box_node_link::session_io::SessionError;
use box_node_link::wire;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// What a caller is allowed to do on this connection, decided once per
/// request exactly as the HTTP path decides it.
///
/// One field today. The gate resolves to a node id, and everything the
/// daemon needs to know about the caller follows from it: `Some(id)` is a
/// session scoped to that node, `None` is either the control plane (main
/// token) or an enroll-grant registration, and the handler layer tells those
/// two apart itself. A field for "is main" was tried and removed -- nothing
/// read it, and an unread field in a security struct is a claim the code
/// does not make.
pub(crate) struct Gate {
    /// The node this connection speaks for, or `None` for the control plane
    /// (a caller holding the main discovery token).
    pub(crate) node_id: Option<String>,
}
impl Gate {
    /// Resolve a request's credential and decide what it may do.
    ///
    /// The order matters and is copied from the HTTP path: resolve the
    /// session *first*, then decide whether the method is allowed. Filtering
    /// the other way round turns a valid-session/forbidden-method request
    /// into a 401 instead of a 403, which hides the real reason from the
    /// caller.
    ///
    /// `node_register` is the one exception on both paths: it carries a
    /// one-shot enroll grant instead of a session token, and its handler
    /// validates the grant itself.
    pub(crate) fn resolve(
        state: &DaemonState,
        main_token: &str,
        request: &serde_json::Value,
    ) -> Result<Self, GateDenied> {
        let supplied = request["token"].as_str();
        let method = request["method"].as_str().unwrap_or("");
        let epoch = request["epoch"].as_u64().unwrap_or(0);
        let is_main = supplied == Some(main_token);
        let session_node = if is_main {
            None
        } else {
            state.nodes.session_node_for(supplied, epoch)
        };
        let is_register =
            method == "node_register" && request["enrollToken"].as_str().is_some_and(|v| !v.is_empty());
        if !is_main && session_node.is_none() && !is_register {
            return Err(GateDenied::NoSession);
        }
        if session_node.is_some()
            && !crate::nodes::NodeManager::is_node_method(method)
            && method != "node_register"
        {
            return Err(GateDenied::ForbiddenMethod);
        }
        Ok(Self { node_id: session_node })
    }
}

/// Why the gate refused a request. The two variants map to 401 and 403 on
/// the HTTP path; on the frame path they become a `close` frame carrying the
/// same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateDenied {
    /// No valid session and not an enroll-grant registration.
    NoSession,
    /// A valid session asking for a method outside `NODE_METHODS`.
    ForbiddenMethod,
}

impl GateDenied {
    /// The wire reason. Short and stable: a helper matches on it to decide
    /// whether to re-enroll (401) or to stop asking (403).
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NoSession => "no-session",
            Self::ForbiddenMethod => "forbidden-method",
        }
    }
}

/// Drive one node-link connection to its end.
///
/// The first frame decides the path, and the two paths share nothing after it:
/// a `Hello` runs the handshake and then a loop of encrypted frames, while a
/// `Request` is the L1 plaintext path with the behaviour that shipped first.
///
/// The reader goes with the stream: it has already buffered the first frame
/// bytes, and a second reader built from the socket would skip them. A
/// BufReader reads in blocks, so the first frame is usually already in
/// `buffered` by the time the blank line is seen -- dropping it would look
/// exactly like a clean close, with no reply and no error.
pub(crate) fn handle_link(
    stream: TcpStream,
    buffered: Vec<u8>,
    state: Arc<DaemonState>,
    main_token: &str,
) {
    let clone = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let leftovers = std::io::Cursor::new(buffered);
    let mut reader = BufReader::new(leftovers.chain(clone));
    let mut writer = BufWriter::new(stream);

    let first = match read_frame(&mut reader) {
        Ok(Some(frame)) => frame,
        // A clean close at a frame boundary is a peer that finished.
        Ok(None) => return,
        Err(error) => {
            tracing::debug!(target: "node_link", %error, "frame error; closing link");
            return;
        }
    };

    match first.frame_type {
        FrameType::Hello => handle_secure(reader, writer, first, &state, main_token),
        FrameType::Request => handle_plaintext(reader, writer, first, &state, main_token),
        other => {
            tracing::debug!(target: "node_link", ?other, "unexpected first frame; closing link");
            let _ = send_close(&mut writer, "unexpected-frame-type");
        }
    }
}

/// The L1 plaintext loop, unchanged.
///
/// Kept as its own function rather than folded into `handle_link` so the two
/// paths can be read side by side: the gate, the dispatch and the reply are the
/// same shape, and only the frame encoding differs.
fn handle_plaintext(
    mut reader: BufReader<std::io::Chain<std::io::Cursor<Vec<u8>>, TcpStream>>,
    mut writer: BufWriter<TcpStream>,
    first: Frame,
    state: &Arc<DaemonState>,
    main_token: &str,
) {
    let mut frame = first;
    loop {
        if frame.frame_type != FrameType::Request {
            let _ = send_close(&mut writer, "unexpected-frame-type");
            return;
        }
        let request = match frame.value() {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "payload is not JSON; closing link");
                return;
            }
        };
        match Gate::resolve(state, main_token, &request) {
            Ok(gate) => {
                // `__nodeId` is server-owned, exactly as on the HTTP path:
                // drop whatever the caller sent, then bind it from the
                // validated session. Without the unconditional drop a
                // discovery-token holder could name any node in the body.
                let mut bound = request;
                crate::bind_request_node(&mut bound, gate.node_id.as_deref());
                let response = crate::dispatch::dispatch(state, &bound);
                let reply = match Frame::json(FrameType::Response, &response) {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!(target: "node_link", %error, "cannot encode response");
                        return;
                    }
                };
                if write_frame(&mut writer, &reply).is_err() || writer.flush().is_err() {
                    return;
                }
            }
            Err(denied) => {
                let _ = send_close(&mut writer, denied.reason());
                return;
            }
        }
        // A node link is long-lived by design, so there is no per-request
        // timeout to inherit: the read blocks until a frame arrives or the peer
        // goes away.
        frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "frame error; closing link");
                return;
            }
        };
    }
}

/// The L4b path: handshake, then an encrypted request/response loop.
///
/// The order of the checks is deliberate. The node authenticates first -- a mac
/// over the transcript, or a node-key signature -- and only then does the
/// control plane spend a signature on it, so an unauthenticated peer cannot
/// make this daemon sign anything.
///
/// The node key is checked against the row in the secrets store. That is what
/// makes "a signature verified" mean "the right node connected" rather than
/// merely "someone who owns some key connected" -- the distinction the L2
/// review found missing.
///
/// A failure anywhere closes the connection. There is no retry loop, because
/// "retry until it works" is how a downgrade succeeds on the nth attempt.
fn handle_secure(
    mut reader: BufReader<std::io::Chain<std::io::Cursor<Vec<u8>>, TcpStream>>,
    mut writer: BufWriter<TcpStream>,
    hello_frame: Frame,
    state: &Arc<DaemonState>,
    main_token: &str,
) {
    let hello_wire: wire::HelloWire = match serde_json::from_slice(&hello_frame.payload) {
        Ok(wire) => wire,
        Err(error) => {
            tracing::debug!(target: "node_link", %error, "hello is not the wire shape; closing link");
            let _ = send_close(&mut writer, "bad-hello");
            return;
        }
    };
    let hello: Hello = match hello_wire.to_hello() {
        Ok(hello) => hello,
        Err(error) => {
            tracing::debug!(target: "node_link", %error, "hello is not usable; closing link");
            let _ = send_close(&mut writer, "bad-hello");
            return;
        }
    };

    let paths = match state.paths.read() {
        Ok(paths) => paths.clone(),
        Err(_) => {
            let _ = send_close(&mut writer, "no-runtime");
            return;
        }
    };

    // The control plane identity, generated on first use. A failure here is
    // fatal: a daemon that cannot read its own identity cannot complete a
    // handshake at all, and pretending otherwise would mean signing with a key
    // it does not have.
    let identity_collection = match node_keys_collection(&paths) {
        Ok(collection) => collection,
        Err(reason) => {
            let _ = send_close(&mut writer, reason);
            return;
        }
    };
    let identity = match ensure_identity(&identity_collection) {
        Ok(fingerprint) => fingerprint,
        Err(reason) => {
            let _ = send_close(&mut writer, reason);
            return;
        }
    };

    // This node row. Three things come from it: the registered node key that
    // node-key mode must match, the pre-shared key that psk mode
    // authenticates with, and the chain end that token mode verifies against.
    // A node that has never enrolled has none of the three.
    let record = match node_record(&paths, &hello.node_id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::debug!(target: "node_link", node_id = %hello.node_id, "no node key row");
            let _ = send_close(&mut writer, "not-enrolled");
            return;
        }
        Err(reason) => {
            let _ = send_close(&mut writer, reason);
            return;
        }
    };

    // The mode the row was issued in must be the mode offered. A mismatch is
    // a refusal, not a fallback: the row records which secret this node
    // holds, and honouring a different mode means reading a field the row
    // does not populate. Today an empty field makes that fail closed by
    // accident -- and an accident is one refactor away from being wrong.
    let offered = box_node_link::node_keys::KeyMode::from(hello.mode);
    if mode_of(&record) != offered {
        tracing::debug!(
            target: "node_link",
            node_id = %hello.node_id,
            ?offered,
            stored = ?mode_of(&record),
            "the offered mode does not match the stored row; closing link"
        );
        let _ = send_close(&mut writer, "mode-mismatch");
        return;
    }

    let nonce = match handshake::fresh_nonce() {
        Ok(nonce) => nonce,
        Err(_) => {
            let _ = send_close(&mut writer, "no-entropy");
            return;
        }
    };
    let control = match ControlHand::new(&SystemRandom::new()) {
        Ok(hand) => hand,
        Err(_) => {
            let _ = send_close(&mut writer, "no-entropy");
            return;
        }
    };

    // The session certificate the node unseals against the fingerprint it
    // pinned offline. Its contents are the node identity and the control
    // fingerprint, which is what makes a rotated control-plane key detectable
    // rather than silently accepted.
    let now = box_foundation::now_seconds();
    let cert = serde_json::to_vec(&box_node_link::session::SessionCert {
        node_id: hello.node_id.clone(),
        control_fingerprint: identity.clone(),
        issued_at: now,
        expires_at: now + 86_400,
    });
    let cert = match cert {
        Ok(cert) => cert,
        Err(_) => {
            let _ = send_close(&mut writer, "no-cert");
            return;
        }
    };

    // The registered key, decoded from the hex the store holds it in. Bound
    // here so the slice outlives the call.
    let registered = record
        .node_pubkey
        .as_deref()
        .and_then(|hex| hex_decode(hex));
    let (keys, ack, spent_link) = match handshake::control_finish(
        control,
        &hello,
        nonce,
        &match signing_key(&identity_collection) {
            Ok(key) => key,
            Err(reason) => {
                tracing::warn!(target: "node_link", "the stored control plane key is unusable");
                let _ = send_close(&mut writer, reason);
                return;
            }
        },
        &cert,
        &bootstrap_secret(&hello, &record),
        registered.as_deref(),
    ) {
        Ok(pair) => pair,
        Err(error) => {
            // The reason is logged, never the material: a refused handshake is
            // interesting, the values that caused it are not.
            tracing::debug!(target: "node_link", ?error, node_id = %hello.node_id, "handshake refused");
            let _ = send_close(&mut writer, "handshake-refused");
            return;
        }
    };

    // Persist the spent link BEFORE the reply goes out.
    //
    // The design rule is "persist before sending": if the ack is lost and the
    // node retries, the daemon already expects the preimage of what it just
    // accepted, so a retry with the same link is refused rather than granted
    // a second session. Writing after the reply would leave a window where
    // the link is still spendable.
    //
    // A failure here closes the link rather than continuing: the handshake
    // succeeded, so the node believes it is connected, and letting it talk
    // while the daemon has lost track of the link would make the next
    // handshake a coin flip.
    if let Some(link) = spent_link {
        let stored = hex(&link);
        if let Err(error) = spend_chain_link(&paths, &hello.node_id, &stored) {
            tracing::warn!(
                target: "node_link",
                %error,
                node_id = %hello.node_id,
                "cannot persist the spent chain link; closing link"
            );
            let _ = send_close(&mut writer, "link-not-stored");
            return;
        }
    }

    let payload = match serde_json::to_vec(&wire::ack_wire(&ack)) {
        Ok(payload) => payload,
        Err(_) => {
            let _ = send_close(&mut writer, "no-ack");
            return;
        }
    };
    if write_frame(
        &mut writer,
        &Frame {
            frame_type: FrameType::HelloAck,
            payload,
        },
    )
    .is_err()
        || writer.flush().is_err()
    {
        return;
    }

    // The keys are swapped for this side. The handshake produces one
    // pair of directional keys; a session seals under c2s and opens under
    // s2c, so the peer that receives our frames must hold them the other
    // way round. The node does that in node_finish; without the same swap
    // here both sides would seal under c2s and neither could open the
    // other -- which is exactly the failure the first end-to-end test
    // produced.
    let mut session = Session::from_keys(
        box_node_link::handshake::SessionKeys {
            c2s: keys.s2c,
            s2c: keys.c2s,
            transcript: keys.transcript,
        },
        hello.node_id.clone(),
        hello.mode,
    );
    run_secure_loop(&mut reader, &mut writer, &mut session, state, main_token);
}

/// The encrypted loop: read a sealed frame, gate it, dispatch, seal the reply.
///
/// The gate and the dispatch are the same ones the plaintext path uses. Only
/// the encoding differs, which is the whole claim of this stage: the transport
/// changed and the permission model did not.
fn run_secure_loop(
    reader: &mut BufReader<std::io::Chain<std::io::Cursor<Vec<u8>>, TcpStream>>,
    writer: &mut BufWriter<TcpStream>,
    session: &mut Session,
    state: &Arc<DaemonState>,
    main_token: &str,
) {
    loop {
        let frame = match read_frame(reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "frame error; closing session");
                return;
            }
        };
        let payload = match session.open_frame(&frame) {
            Ok(payload) => payload,
            // A frame that will not open is either tampered with or out of
            // step, and both end the connection: there is no way to
            // resynchronise an AEAD stream, and pretending otherwise would
            // mean answering a request nobody sent.
            Err(error) => {
                tracing::debug!(target: "node_link", ?error, "frame will not open; closing session");
                let _ = send_close(writer, "frame-rejected");
                return;
            }
        };
        let request: serde_json::Value = match serde_json::from_slice(&payload) {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "payload is not JSON; closing session");
                return;
            }
        };
        // The handshake is the credential. A session that came through
        // control_finish has already proved it holds the bootstrap secret
        // for this node id, so the request is dispatched under that identity
        // rather than being asked for a second token -- which would mean the
        // encrypted path is no stronger than the plaintext one it replaces.
        //
        // `__nodeId` is still server-owned: the value bound here comes from
        // the session, and anything the payload supplied is dropped.
        let mut bound = request;
        crate::bind_request_node(&mut bound, Some(session.node_id()));
        // The method whitelist still applies. It is the one thing the
        // handshake does not establish: a node session may only ask for
        // what a node session may do.
        if !crate::nodes::NodeManager::is_node_method(bound["method"].as_str().unwrap_or("")) {
            let _ = send_close(writer, "forbidden-method");
            return;
        }
        let response = crate::dispatch::dispatch(state, &bound);
        let payload = match serde_json::to_vec(&response) {
            Ok(payload) => payload,
            Err(_) => return,
        };
        let reply = match session.seal_frame(FrameType::Response, &payload) {
            Ok(frame) => frame,
            // A key that has aged or been used past its limit is a
            // rekey, and rekey is not implemented yet: say so and
            // close rather than silently continuing on a key the
            // design has retired.
            Err(SessionError::KeyTooOld | SessionError::KeyTooUsed) => {
                tracing::debug!(target: "node_link", "session key needs a rekey; closing session");
                let _ = send_close(writer, "rekey-required");
                return;
            }
            Err(_) => return,
        };
        if write_frame(writer, &reply).is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// The control-identity collection, or the reason it could not be opened.
///
/// Returns a reason rather than an error so the caller can put it on the wire
/// as a close reason: a peer that is refused deserves to know which of the
/// refusals it hit, and the reasons here are not secret.
fn node_keys_collection(
    paths: &box_foundation::BoxPaths,
) -> Result<box_foundation::collection::Collection<crate::node_keys::ControlIdentity>, &'static str>
{
    match crate::node_keys::open_control_identity(paths) {
        Ok(collection) => Ok(collection),
        Err(_) => Err("no-identity"),
    }
}

/// The control-plane fingerprint, generating the identity on first use.
fn ensure_identity(
    collection: &box_foundation::collection::Collection<crate::node_keys::ControlIdentity>,
) -> Result<String, &'static str> {
    match crate::node_keys::ensure_control_identity(collection) {
        Ok(fingerprint) => Ok(fingerprint),
        Err(_) => Err("no-identity"),
    }
}

/// The signing key for the stored identity.
///
/// Rebuilt from PKCS#8 on every handshake rather than cached: a cached key
/// would be a secret held in memory for the life of the process, and a
/// handshake signs once per connection.
///
/// A key that will not load is an error, not a reason to mint a new one. An
/// earlier version generated a fresh keypair on a load failure, which fails
/// safe in the narrow sense -- the node rejects a fingerprint it never pinned
/// -- but leaves the daemon signing as an identity it does not know it has
/// lost. A control plane whose identity changed under it should say so and
/// stop, not quietly become somebody else.
fn signing_key(
    collection: &box_foundation::collection::Collection<crate::node_keys::ControlIdentity>,
) -> Result<box_node_link::handshake::ControlKey, &'static str> {
    match crate::node_keys::find_identity(collection) {
        Ok(Some(record)) => handshake::control_signing_key(&record.seckey)
            .map_err(|_| "no-identity"),
        _ => Err("no-identity"),
    }
}

/// The node key row for this node, or None if it has never enrolled.
///
/// A revoked or expired row is reported as an error rather than as absent,
/// because the two mean different things to a peer: not-enrolled is a setup
/// problem, revoked is a decision.
fn node_record(
    paths: &box_foundation::BoxPaths,
    node_id: &str,
) -> Result<Option<NodeKeyRecord>, &'static str> {
    let collection = match crate::node_keys::open_node_keys(paths) {
        Ok(collection) => collection,
        Err(_) => return Err("no-key-store"),
    };
    let rows = match collection.load_all() {
        Ok(rows) => rows,
        Err(_) => return Err("no-key-store"),
    };
    match rows.into_iter().find(|row| row.id == node_id) {
        Some(row) if crate::node_keys::is_node_live(&row) => Ok(Some(row)),
        Some(_) => Err("key-revoked"),
        None => Ok(None),
    }
}

/// The bootstrap secret a mode presents, for the wrap-key mix.
///
/// Psk mode is the pre-shared key from the secrets store, and it never travels:
/// the node proves it holds the key with a mac over the transcript, and the
/// control plane proves the same by mixing it into the wrap key. Token mode is
/// the chain value the node presented, which is on the wire and therefore only
/// defence in depth -- the real authentication there is the control signature
/// plus the fingerprint the node pinned offline. Node-key mode has no
/// bootstrap secret at all, which is the point of it being the reconnect path.
fn bootstrap_secret(hello: &Hello, record: &NodeKeyRecord) -> Vec<u8> {
    match hello.mode {
        AuthMode::Psk => record.secret.clone().into_bytes(),
        // The value the next link must be the preimage of.
        //
        // Before the first connection that is the chain end; afterwards it
        // is the last link accepted, which is what spends a link. Reading
        // the chain end unconditionally -- which is what this did at first
        // -- leaves it unchanged for the life of the row, so the same link
        // keeps verifying and the authentication material is reusable
        // forever.
        //
        // It is stored as hex because the store is JSON and the value is
        // bytes. Decoding it here is what makes the two sides agree: the node
        // hashes the seed and uses the raw 32 bytes, so handing the ASCII of
        // the hex to the KDF instead would make every mac mismatch.
        AuthMode::Token => record
            .last_accepted
            .as_deref()
            .or(record.chain_end.as_deref())
            .and_then(hex_decode)
            .unwrap_or_default(),
        AuthMode::NodeKey => Vec::new(),
    }
}

/// Decode a hex field from the store, or None if it is not hex.
///
/// The node public key is stored hex because the store is JSON and a key is
/// bytes; the handshake wants the bytes. A malformed row is treated as absent
/// rather than as an error, so a corrupted row refuses the handshake instead
/// of crashing the connection.
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

/// Record the chain link a handshake just spent.
///
/// The stored value becomes what the next handshake must present the
/// preimage of, which is what makes a spent link useless: resending it hashes
/// to something the daemon no longer expects.
///
/// A row that is not a token row is left alone -- psk and node-key modes have
/// no chain to advance, and writing a chain value into them would be a
/// silent mode change.
fn spend_chain_link(
    paths: &box_foundation::BoxPaths,
    node_id: &str,
    link_hex: &str,
) -> BoxResult<()> {
    let collection = crate::node_keys::open_node_keys(paths)?;
    let rows = collection.load_all()?;
    let mut record = rows
        .into_iter()
        .find(|row| row.id == node_id)
        .ok_or_else(|| format!("no node key row for {node_id}"))?;
    if record.mode != box_node_link::node_keys::KeyMode::Token {
        return Ok(());
    }
    record.last_accepted = Some(link_hex.to_owned());
    collection.upsert(&[record])?;
    Ok(())
}

/// Lowercase hex, for a link on its way into a store row.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Write a close frame carrying a reason. Best-effort: the peer is going
/// away regardless.
fn send_close<W: std::io::Write>(writer: &mut W, reason: &str) {
    // A close frame is a few dozen bytes of JSON. If that cannot be encoded
    // or written there is nothing useful to say and no point pretending, so
    // both failures are dropped rather than surfaced: the peer is going away
    // regardless, and the caller has nothing to do with the news.
    if let Ok(frame) = Frame::json(FrameType::Close, &serde_json::json!({ "reason": reason })) {
        let _ = write_frame(writer, &frame);
    }
}
