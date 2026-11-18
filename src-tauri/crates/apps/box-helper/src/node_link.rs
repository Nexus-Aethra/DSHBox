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
//! - **It does not persist the node key.** The long-term key is generated per
//!   process and registered on the first handshake, so a restart re-enrols.
//!   Persisting it is stage B, and it needs a 0600 file the helper can write
//!   on every target it runs on.
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

/// What the helper needs to open a session, and where it came from.
///
/// The seed and the pinned fingerprint both arrive with the enrol reply and
/// exist nowhere else afterwards -- the daemon keeps only the chain end, by
/// design, so a helper that drops them cannot reconnect. That is why they are
/// fields on a struct the caller holds rather than something re-fetched.
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
    control_pubkey: Vec<u8>,
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
    let link = box_node_link::chain::hash_n(&seed, config.chain_length.saturating_sub(1).max(1));

    // The node long-term key, generated here rather than loaded: this build
    // has no key file. The first handshake registers it with the daemon and
    // every one after is checked against it.
    let node_key = handshake::generate_control_keypair()
        .map_err(|e| format!("cannot generate a node key: {e}"))?;
    let node_pubkey = box_node_link::handshake::hex_decode(&node_key.pubkey_hex)
        .ok_or("the generated node key is not valid hex")?;
    let node_signing = handshake::control_signing_key(&node_key.seckey_hex)
        .map_err(|e| format!("cannot load the generated node key: {e}"))?;

    let rng = box_node_link::SystemRandom::new();
    let node_hand =
        NodeHand::new(&rng, control_pubkey).map_err(|e| format!("handshake: {e}"))?;

    // Node-key mode. The signature is over t1, the client half, which is why
    // it can travel in the hello at all -- signing t2 would need the ack
    // first, and the protocol is one frame.
    let mut hello = Hello {
        node_id: node_id.to_owned(),
        mode: AuthMode::NodeKey,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().map_err(|e| format!("entropy: {e}"))?,
        eph_a: node_hand.public_key().map_err(|e| format!("handshake: {e}"))?,
        mac: None,
        node_pubkey: Some(node_pubkey),
        node_sig: None,
        link: None,
    };
    let t1 = handshake::transcript_client(&hello);
    hello.node_sig = Some(node_signing.sign(&t1).as_ref().to_vec());

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
        let reason = frame
            .value()
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", frame.frame_type));
        return Err(format!("the control plane refused the handshake: {reason}"));
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
        AuthMode::NodeKey,
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
        let request = serde_json::json!({ "method": method, "params": params });
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
            let reason = reply
                .value()
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("{:?}", reply.frame_type));
            return Err(format!("the control plane refused: {reason}"));
        }
        let opened = self
            .session
            .open_frame(&reply)
            .map_err(|e| format!("open: {e}"))?;
        serde_json::from_slice(&opened).map_err(|e| format!("decode: {e}"))
    }
}

/// Decode the control-plane public key from the hex the enrol reply carries.
pub fn control_pubkey_hex_to_bytes(hex: &str) -> Vec<u8> {
    box_node_link::handshake::hex_decode(hex).unwrap_or_default()
}
