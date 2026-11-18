//! The node link client: a long-lived encrypted session to the control plane.
//!
//! This is the node side of what the daemon serves. The helper registers over
//! HTTP exactly as it did in stage A -- the enrol grant and the session token
//! are the bootstrap material -- and then opens a `/link` connection, runs the
//! handshake, and sends its heartbeats over the encrypted frames instead of
//! one HTTP POST each.
//!
//! Three things this deliberately does not do:
//!
//! - **It does not replace the HTTP path.** The daemon still answers `/rpc`,
//!   and the helper still uses it for anything that is not a node method. Two
//!   ways to reach the same handler is a migration, not a rewrite.
//! - **It does not persist a node key, and does not need one yet.** The
//!   helper connects in token mode, where the chain is the reconnect
//!   credential: each handshake spends one link and the row holds the last
//!   accepted one, so a restart spends the next link down. Node-key mode
//!   needs a key that survives a restart, which means a 0600 file the helper
//!   can write on every target it runs on -- that is stage B, and until then
//!   the chain is what reconnects.
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
#[derive(Default)]
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
) -> Result<Link, String> {
    let (host, port) = crate::split_url(control_url)?;
    let stream = TcpStream::connect((host.as_str(), port)).map_err(|e| format!("connect: {e}"))?;

    // The chain link this handshake spends. The daemon holds the chain end
    // and accepts a value only when H(value) equals it, so the first value is
    // c_(n-1): sending c_n itself would be a value whose hash nothing will
    // ever accept. That is the off-by-one the chain walk already warns about.
    let seed = box_node_link::handshake::hex_decode(&config.seed)
        .ok_or("the seed is not valid hex")?;
    let seed: [u8; 32] = seed.try_into().map_err(|_| "the seed is not 32 bytes")?;
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
    let link = box_node_link::chain::hash_n(&seed, config.chain_length - 1);

    // Token mode, not node-key. The chain is the reconnect credential: every
    // handshake spends one link, and the row holds 64 of them, so a restart
    // reconnects until the chain runs out.
    //
    // Node-key mode would need the long-term key to survive a restart, and
    // this build has no key file -- the key is generated per process, so a
    // restart offers a key the daemon has never seen and the handshake is
    // refused forever. Persisting the key is stage B and needs a 0600 file
    // the helper can write on every target it runs on. Until then the chain
    // is what reconnects, which is exactly what it is for.
    // The control-plane key comes from the config, which is where the enrol
    // reply puts it. A bad hex here is a typo in the flag, and saying so is
    // better than a signature-shaped error three steps later.
    let control_pubkey = control_pubkey_hex_to_bytes(&config.control_pubkey)
        .ok_or_else(|| "the control-plane public key is not valid hex".to_owned())?;

    let rng = box_node_link::SystemRandom::new();
    let node_hand =
        NodeHand::new(&rng, control_pubkey).map_err(|e| format!("handshake: {e}"))?;

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
    })
}

impl Link {
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

        let reply = read_frame(&mut self.reader).map_err(|e| format!("read: {e}"))?;
        let reply = match reply {
            Some(frame) => frame,
            None => return Err("the control plane closed the link".to_owned()),
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
