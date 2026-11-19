//! The node link client: a long-lived encrypted session to the control plane.
//!
//! This is the node side of what the daemon serves. The helper registers over
//! HTTP exactly as it did in stage A -- the enrol grant and the session token
//! are the bootstrap material -- and then opens a `/link` connection, runs the
//! handshake, and sends its heartbeats over the encrypted frames instead of
//! one HTTP POST each.
//!
//! The shape of this module, and the one limit it still has:
//!
//! - **It does not replace the HTTP path.** The daemon still answers `/rpc`,
//!   and the helper still uses it for anything that is not a node method. Two
//!   ways to reach the same handler is a migration, not a rewrite.
//! - **The walk is persisted when the operator gives a state directory**,
//!   which is what satisfies the design red line ("persist before sending",
//!   section 3): the position is written *before* the hello goes out, so a
//!   restart continues from where the process got to instead of offering
//!   c_(n-1) again -- a value the control plane already holds and refuses.
//!   Without the directory the walk lives only in memory and a restart
//!   re-enrolls, exactly as stage A did. The two are kept apart on purpose:
//!   the seed is a secret the operator manages, this is bookkeeping the
//!   helper owns.
//! - **It does not drive tasks.** Stage B moves operations down; this is the
//!   transport they will arrive on.
//!
//! The handshake is driven here rather than through `session_io::node_connect`,
//! because that function takes the socket and does not give the writer back --
//! and a session that cannot write is not a session. The steps are the same
//! ones it performs, in the same order, with the same checks.

use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
use box_node_link::handshake::{self, AuthMode, Hello, NodeHand};
use box_node_link::session_io::Session;
use box_node_link::wire;
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;

/// The bootstrap material for the link.
///
/// Deliberately does not carry the node id: that comes from the args the
/// caller already parsed, and a config that has to be patched after
/// construction is a config that can be used unpatched.
#[derive(Default, Clone)]
pub struct LinkConfig {
    /// The token-chain seed, hex. Spent one link per handshake.
    pub seed: String,
    /// The control-plane fingerprint the operator pinned offline.
    pub pinned_fingerprint: String,
    /// The control-plane public key, hex. Carried in the enrol reply so the
    /// node can build the handshake without a second fetch.
    pub control_pubkey: String,
    /// How many links the chain has, from the same reply. Used to compute
    /// the first link the daemon will accept.
    pub chain_length: u32,
}

/// An open session: the handshake is done and frames can be exchanged.
pub struct Link {
    session: Session,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    /// The chain walk, held across reconnects within one process.
    ///
    /// Not rebuilt per connection: a fresh walk starts at `c_(n-1)` again,
    /// and the control plane already holds that value, so the second
    /// handshake would be refused. This is what makes a reconnect spend the
    /// *next* link rather than the first one twice.
    ///
    /// It is persisted across processes when a state directory is supplied,
    /// which is what lets a restart reconnect rather than re-enroll.
    walker: box_node_link::chain::ChainNode,
    /// Everything needed to open the link again.
    config: LinkConfig,
    node_id: String,
    control_url: String,
    state_dir: Option<std::path::PathBuf>,
}


/// The file the walk position is persisted in, next to the seed the operator
/// supplied. Its name is deliberately not the seed name: the seed is a secret
/// the operator manages, this is bookkeeping the helper owns.
const WALK_FILE: &str = "node-link-walk.json";

/// Read the persisted walk position, or None if there is nothing to read.
///
/// A file that is missing, empty or unparseable is "no state", not an error:
/// a first run has no state, and a truncated file from a crash is exactly the
/// case the smaller-k rule exists for. Failing here would turn a recoverable
/// crash into a helper that refuses to start.
pub fn read_walk(directory: &std::path::Path) -> Option<u32> {
    let text = std::fs::read_to_string(directory.join(WALK_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("next_k")?.as_u64().map(|k| k as u32)
}

/// Persist the walk position. The write is atomic, because a half-written
/// state file is worse than none: it would be read as a position the helper
/// never reached.
pub fn write_walk(directory: &std::path::Path, next_k: u32) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|e| format!("state dir: {e}"))?;
    let text = serde_json::to_string(&serde_json::json!({ "next_k": next_k }))
        .map_err(|e| format!("state encode: {e}"))?;
    let path = directory.join(WALK_FILE);
    // A k-suffixed temp name, so two helpers sharing a state directory do not
    // race on the rename. One helper per directory is the documented shape,
    // but a fixed name makes the second one corrupt the first silently.
    let temp = directory.join(format!("{WALK_FILE}.{next_k}.tmp"));
    std::fs::write(&temp, text).map_err(|e| format!("state write: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("state rename: {e}"))?;
    Ok(())
}
/// Build the walk a process starts with.
///
/// Resumes below the persisted position when there is one, because the file
/// holds the link that was SPENT and the control plane already holds that
/// value -- resuming at it re-offers it and is refused forever. That is the
/// whole point of persisting, and it is a function of its own so the test
/// can drive it: the decision lives between two files, and a decision in the
/// gap is a decision nothing checks.
pub fn resume_walk(
    seed: [u8; 32],
    chain_length: u32,
    state_dir: Option<&std::path::Path>,
) -> box_node_link::chain::ChainNode {
    match state_dir.and_then(read_walk) {
        Some(spent_k) => {
            box_node_link::chain::ChainNode::at(seed, spent_k.saturating_sub(1).max(1), chain_length)
        }
        None => box_node_link::chain::ChainNode::new(seed, chain_length),
    }
}

/// Open a socket to the control plane.
///
/// Separate from `connect` so a test can hand `connect` a socket it dialled
/// itself, against a peer it controls -- which is what makes the handshake
/// and its failure paths reachable at all.
pub fn dial(control_url: &str) -> Result<TcpStream, String> {
    let (host, port) = crate::split_url(control_url)?;
    TcpStream::connect((host.as_str(), port)).map_err(|e| format!("connect: {e}"))
}

/// Open the link: handshake, then a session ready to carry frames.
///
/// `control_url` is `http://host:port`; the link is the same host and port at
/// path `/link`. `chain_length` is the length the enrol reply reported, used to
/// compute the first link the daemon will accept.
pub fn connect(
    config: &LinkConfig,
    node_id: &str,
    control_url: &str,
    walker: box_node_link::chain::ChainNode,
    stream: TcpStream,
    state_dir: Option<&std::path::Path>,
) -> Result<Link, String> {
    // The socket is dialled by the caller, not here. That is what makes the
    // handshake testable: a fake peer on the other end of a listener is enough
    // to drive connect to completion, and enough to drive it to a refusal.
    //
    // The chain link this handshake spends. The daemon holds the chain end
    // and accepts a value only when H(value) equals it, so the first value is
    // c_(n-1): sending c_n itself would be a value whose hash nothing will
    // ever accept. That is the off-by-one the chain walk already warns about.
    // A chain shorter than two has no spendable link that is not the seed,
    // and a one-link chain would send the seed itself. The CLI already
    // refuses it; this is the same rule one layer down, stated rather than
    // floored, because a silent floor is how a caller gets a link it did not
    // ask for.
    if config.chain_length < 2 {
        return Err(format!(
            "chain length must be at least 2, got {}",
            config.chain_length
        ));
    }
    // The link comes from the walk, not from `chain_length - 1`: on a
    // reconnect the control plane already holds the first link, and sending
    // it again is refused.
    let mut walker = walker;
    // Persist BEFORE sending. The control plane records its spend before it
    // writes the ack, so a hello that goes out and is answered by silence
    // leaves it holding a value this side never saw accepted. Writing the
    // walk position first is what makes the next attempt use a smaller k --
    // the design red line, and the one thing the in-memory walk cannot do
    // on its own.
    let (hex, k) = walker.next_value();
    if let Some(directory) = state_dir {
        // A failure here is fatal for THIS attempt, and deliberately so. The
        // link is about to be spent on the wire; if the position is not on
        // disk first, the next restart re-offers it and is refused forever --
        // the exact scenario this write exists to prevent. The daemon makes
        // the same choice from the other side: spend_chain_link failing
        // sends close{link-not-stored} and refuses to ack.
        //
        // The cost is bounded, because open_link treats a failed connect as
        // non-fatal and falls back to HTTP: a full disk costs the encrypted
        // path, not the node.
        write_walk(directory, k).map_err(|e| format!("cannot persist the chain walk: {e}"))?;
    }
    let link = box_node_link::handshake::hex_decode(&hex)
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .expect("a chain value is 32 bytes of hex");

    // Token mode. The chain is the reconnect credential: each handshake
    // spends one link, and the walk holds where it got to -- in memory for
    // this process, and on disk when the operator gave a state directory,
    // which is what makes a restart reconnect rather than re-enrol.
    // The control-plane key comes from the config, which is where the enrol
    // reply puts it. A bad hex here is a typo in the flag, and saying so is
    // better than a signature-shaped error three steps later.
    let control_pubkey = control_pubkey_hex_to_bytes(&config.control_pubkey)
        .ok_or_else(|| "the control-plane public key is not valid hex".to_owned())?;

    let rng = box_node_link::SystemRandom::new();
    let node_hand =
        NodeHand::new(&rng, control_pubkey.clone()).map_err(|e| format!("handshake: {e}"))?;

    let mut hello = Hello {
        node_id: node_id.to_owned(),
        mode: AuthMode::Token,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().map_err(|e| format!("entropy: {e}"))?,
        eph_a: node_hand.public_key().map_err(|e| format!("handshake: {e}"))?,
        mac: None,
        node_pubkey: None,
        node_sig: None,
        link: Some(link),
    };
    // The mac proves the node holds the bootstrap secret: HMAC over t1 with
    // the key derived from the link being spent.
    let t1 = handshake::transcript_client(&hello);
    let mac = box_node_link::kdf::hmac_sha256(&box_node_link::kdf::bootstrap_key(&link), &t1);
    hello.mac = Some(mac);

    let mut writer = BufWriter::new(stream.try_clone().map_err(|e| format!("socket: {e}"))?);
    let mut reader = BufReader::new(stream);

    // 1. The hello. Handshake frames are not encrypted: the encryption is
    //    established by them.
    let payload = serde_json::to_vec(&wire::hello_wire(&hello))
        .map_err(|e| format!("cannot encode the hello: {e}"))?;
    write_frame(
        &mut writer,
        &Frame {
            frame_type: FrameType::Hello,
            payload,
        },
    )
    .map_err(|e| format!("cannot send the hello: {e}"))?;
    writer.flush().map_err(|e| format!("socket: {e}"))?;

    // 2. The ack. Anything else is a refusal, and the reason is surfaced
    //    rather than swallowed: a caller deciding whether to retry needs to
    //    know which refusal it hit.
    let frame = read_frame(&mut reader).map_err(|e| format!("cannot read the ack: {e}"))?;
    let frame = match frame {
        Some(frame) => frame,
        None => return Err("the control plane hung up before answering".to_owned()),
    };
    if frame.frame_type != FrameType::HelloAck {
        return Err(format!(
            "the control plane refused the handshake: {}",
            refusal_reason(&frame)
        ));
    }
    let ack_wire: wire::HelloAckWire = serde_json::from_slice(&frame.payload)
        .map_err(|e| format!("the ack is not the wire shape: {e}"))?;
    let ack = ack_wire.to_ack().map_err(|e| format!("the ack is malformed: {e}"))?;

    // 3. Finish: verify the control signature, agree, derive the keys, and
    //    unseal the certificate against the fingerprint the operator pinned.
    //    The last step is what makes this mutually authenticated rather than
    //    trust-on-first-use.
    let outcome = handshake::node_finish(node_hand, &hello, &ack, &link)
        .map_err(|e| format!("the handshake did not finish: {e}"))?;
    handshake::node_unseal(
        &outcome.keys,
        &hello,
        &ack,
        &link,
        &outcome.ecdh,
        &config.pinned_fingerprint,
    )
    .map_err(|e| format!("the control plane is not the one pinned: {e}"))?;

    // 4. The session. The node holds the keys the way the protocol builds
    //    them: its send key is the daemon receive key, and the daemon swapped
    //    its own pair when it built its session.
    let session = Session::from_keys(
        outcome.keys,
        node_id.to_owned(),
        AuthMode::Token,
    );
    Ok(Link {
        session,
        reader,
        writer,
        walker,
        config: config.clone(),
        node_id: node_id.to_owned(),
        control_url: control_url.to_owned(),
        state_dir: state_dir.map(std::path::Path::to_path_buf),
    })
}

/// Step the walk back after a failed attempt.
///
/// A free function rather than a method on `Link`, because a `Link` only
/// exists after a *successful* handshake -- so the failure path that needs
/// this is exactly the path a method could not be called from. That is what
/// made the rewind untestable for one round, and why the reviewer deleting it
/// left the suite green.
pub fn rewind_after_failure(walker: &mut box_node_link::chain::ChainNode, offered_k: u32) {
    walker.rewind_to(offered_k.saturating_sub(1).max(1));
}

impl Link {

    /// Open the link again after it dropped, spending the next chain link.
    ///
    /// This is the piece that was missing: a link that stopped working used to
    /// mean HTTP for the rest of the process life, because nothing could open
    /// it again -- the enrol material the first connect needed was gone, and a
    /// fresh chain value would have been the first one, which the control plane
    /// already holds.
    ///
    /// Both are solved by keeping them on the Link: the config and the walk
    /// survive, so a reconnect is an ordinary handshake one link further down
    /// the chain. When the chain runs out the error says so, and the caller
    /// needs a fresh enrol grant -- which is a real limit, not a silent one.
    ///
    /// **A failed attempt costs a link, permanently.** The rewind below steps
    /// back one on any failure, including a refusal, which is a link the daemon
    /// never spent. That errs toward the safe side -- not rewinding after a
    /// lost ack is a permanent refusal, rewinding after a refusal only wastes
    /// budget -- but it means a node on a flaky network does not get the full
    /// 63 handshakes. Measured: with one failure in three attempts, 62.
    pub fn reconnect(&mut self) -> Result<(), String> {
        if self.walker.is_exhausted() {
            return Err(
                "the bootstrap chain is exhausted; a fresh enrol grant is required".to_owned(),
            );
        }

        // The link this attempt will offer, remembered so a failure can rewind
        // to it. The control plane records its spend *before* writing the ack,
        // so a lost ack leaves the daemon holding a value this side has not
        // seen accepted -- and resending that same value is a permanent
        // refusal, because H(c_k) is c_(k+1) and the daemon now expects c_k.
        // That is the design red line, and rewind is the repair it names.
        let offered_k = self.walker.remaining().max(1);
        // Dial, then hand over the socket. A dial that fails rewinds too:
        // the daemon never saw a hello, so nothing was spent, but the
        // conservative direction is the same one and the budget cost is one
        // link either way.
        let stream = match dial(&self.control_url) {
            Ok(stream) => stream,
            Err(error) => {
                rewind_after_failure(&mut self.walker, offered_k);
                return Err(error);
            }
        };
        let reopened = connect(
            &self.config,
            &self.node_id,
            &self.control_url,
            self.walker.clone(),
            stream,
            self.state_dir.as_deref(),
        );
        let reopened = match reopened {
            Ok(link) => link,
            Err(error) => {
                rewind_after_failure(&mut self.walker, offered_k);
                return Err(error);
            }
        };
        *self = reopened;
        Ok(())
    }

    /// Where the walk currently is.
    ///
    /// Only the tests read it: they assert that a failed reconnect stepped the
    /// walk back, which is otherwise unobservable from outside. Marked
    /// `allow(dead_code)` because a `cargo build` does not compile the tests,
    /// so it reads as unused -- and deleting it on that evidence is how the
    /// rewind lost its only witness.
    #[allow(dead_code)]
    pub fn walk_k(&self) -> u32 {
        self.walker.remaining().max(1)
    }

    /// One encrypted request, and the decrypted response.
    ///
    /// The method must be on `NODE_METHODS`: the daemon enforces it on the
    /// encrypted path exactly as it does on the HTTP one, and a method outside
    /// the list closes the link rather than answering it.
    pub fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        // Flat, not nested under `params`. Every node handler reads its
        // arguments off the request root -- `node_heartbeat` reads
        // `request["epoch"]` -- so an envelope that puts them under a key
        // the handler never looks at makes every field null and every call
        // fail. A nested shape is what the generic task route expects, which
        // is how the two came to differ.
        let mut request = serde_json::json!({ "method": method });
        if let Some(object) = request.as_object_mut() {
            if let Some(fields) = params.as_object() {
                for (key, value) in fields {
                    object.insert(key.clone(), value.clone());
                }
            }
        }
        let payload = serde_json::to_vec(&request).map_err(|e| format!("encode: {e}"))?;
        let frame = self
            .session
            .seal_frame(FrameType::Request, &payload)
            .map_err(|e| format!("seal: {e}"))?;
        write_frame(&mut self.writer, &frame).map_err(|e| format!("write: {e}"))?;
        self.writer.flush().map_err(|e| format!("flush: {e}"))?;

        // Read until the response, consuming rekeys on the way.
        //
        // This is also what makes a *quiet* session follow a proactive
        // rotation. The daemon rotates when its keys near their limit, with
        // no request outstanding, so the frame sits in the socket buffer
        // until something reads it -- and something does: the heartbeat is
        // periodic, so the frame is consumed on the next tick rather than
        // never. Without that cadence a quiet session would rotate and
        // never find out.
        //
        // A rekey frame can arrive here, and not only in reply to something:
        // the daemon rotates proactively when its keys near their limit, so a
        // quiet session gets one with no request outstanding. Treating it as a
        // refusal -- which is what this did at first -- drops the link to HTTP
        // exactly when the daemon was trying to keep the session alive.
        let reply = loop {
            let frame = read_frame(&mut self.reader).map_err(|e| format!("read: {e}"))?;
            let Some(frame) = frame else {
                return Err("the control plane closed the link".to_owned());
            };
            if frame.frame_type == FrameType::Rekey {
                self.session
                    .recv_rekey(&frame)
                    .map_err(|e| format!("the rekey was refused: {e}"))?;
                continue;
            }
            break frame;
        };
        if reply.frame_type != FrameType::Response {
            return Err(format!("the control plane refused: {}", refusal_reason(&reply)));
        }
        let opened = self
            .session
            .open_frame(&reply)
            .map_err(|e| format!("open: {e}"))?;
        serde_json::from_slice(&opened).map_err(|e| format!("decode: {e}"))
    }
}

/// Decode the control-plane public key from the hex the enrol reply carries.
///
/// Returns None on bad hex rather than an empty vector: an empty key is
/// accepted by `NodeHand::new` and then fails as a signature mismatch, which
/// is a hex typo wearing a crypto error costume.
pub fn control_pubkey_hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    box_node_link::handshake::hex_decode(hex)
}

/// The reason a close frame carries, or its frame type if it carries none.
///
/// Shared by the handshake and the call path: two copies of "read the reason
/// out of a close frame" is two places for the extraction to drift.
fn refusal_reason(frame: &Frame) -> String {
    frame
        .value()
        .ok()
        .and_then(|v| v["reason"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{:?}", frame.frame_type))
}
