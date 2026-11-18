//! The node link client against a peer that is not the daemon.
//!
//! `connect` and `Link::call` need something on the other end of the socket,
//! and a bin crate cannot construct the daemon state. But the handshake is a
//! *pure* function -- `handshake::control_finish` takes a hello and returns an
//! ack, with no daemon involved -- so a peer can be built from it directly.
//!
//! That is what this file does, and it is not a formality: the first version
//! of `Link::call` nested its arguments under `params`, which the node
//! handlers never read, so every call failed on its first request and the
//! helper fell back to HTTP permanently. A test with a fake peer catches that
//! in a second; a suite with no client-side test did not catch it at all.

use super::node_link::{self, LinkConfig};
use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
use box_node_link::handshake::{self, AuthMode, ControlHand};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;

/// A peer that completes the handshake and answers one request.
///
/// Built from `control_finish`, the same pure function the daemon calls, so
/// the ack is a real one rather than a fixture that agrees with whatever the
/// client happens to build.
struct Peer {
    port: u16,
}

impl Peer {
    fn spawn(seed: [u8; 32], chain_length: u32, control_seckey: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            let mut reader = BufReader::new(stream);

            // 1. The hello.
            let Some(frame) = read_frame(&mut reader).unwrap() else {
                return;
            };
            assert_eq!(frame.frame_type, FrameType::Hello);
            let wire: box_node_link::wire::HelloWire =
                serde_json::from_slice(&frame.payload).unwrap();
            let hello = wire.to_hello().unwrap();

            // 2. The control half of the handshake, against the link the
            //    node should have derived from the seed.
            let link = box_node_link::chain::hash_n(&seed, chain_length - 1);
            let signing = handshake::control_signing_key(&control_seckey).unwrap();
            let cert = serde_json::to_vec(&box_node_link::session::SessionCert {
                node_id: hello.node_id.clone(),
                control_fingerprint: "fp-test".to_owned(),
                issued_at: 1,
                expires_at: 2,
            })
            .unwrap();
            let nonce = handshake::fresh_nonce().unwrap();
            let control = ControlHand::new(&box_node_link::SystemRandom::new()).unwrap();
            let (keys, ack, _spent) = handshake::control_finish(
                control,
                &hello,
                nonce,
                &signing,
                &cert,
                // The stored value is the chain end: what the presented
                // link hashes to. Passing the link itself is the off-by-one
                // the chain walk already warns about.
                &box_node_link::chain::hash_once(&link),
                None,
                false,
            )
            .expect("the peer accepts the handshake");

            // 3. The ack. The peer holds the keys the way the daemon does:
            //    swapped, so its receive key is the node send key.
            let payload = serde_json::to_vec(&box_node_link::wire::ack_wire(&ack)).unwrap();
            write_frame(
                &mut writer,
                &Frame {
                    frame_type: FrameType::HelloAck,
                    payload,
                },
            )
            .unwrap();
            writer.flush().unwrap();

            let mut session = box_node_link::session_io::Session::from_keys(
                box_node_link::handshake::SessionKeys {
                    c2s: keys.s2c,
                    s2c: keys.c2s,
                    transcript: keys.transcript,
                },
                hello.node_id.clone(),
                AuthMode::Token,
            );

            // 4. The request. This is where the envelope shape is pinned: the
            //    peer reads the arguments the way a node handler does, flat
            //    off the request root, and a client that nests them under a
            //    `params` key gets nothing.
            let Some(frame) = read_frame(&mut reader).unwrap() else {
                return;
            };
            let payload = session.open_frame(&frame).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            // A node handler reads `request["epoch"]`, not
            // `request["params"]["epoch"]`.
            assert_eq!(
                request["epoch"].as_u64(),
                Some(4242),
                "the arguments must be flat off the request root"
            );

            let reply = serde_json::json!({ "ok": true, "echo": request["epoch"] });
            let sealed = session
                .seal_frame(FrameType::Response, &serde_json::to_vec(&reply).unwrap())
                .unwrap();
            write_frame(&mut writer, &sealed).unwrap();
            writer.flush().unwrap();
        });
        Peer { port }
    }
}

/// The client half: handshake, then one encrypted request and response.
#[test]
fn a_helper_opens_a_session_and_exchanges_an_encrypted_request() {
    let seed = [9u8; 32];
    let chain_length = 64u32;
    let (control_pubkey, control_seckey) = control_keypair();
    let peer = Peer::spawn(seed, chain_length, control_seckey);

    // The control-plane key the peer generated is not knowable in advance, so
    // the peer is spawned with a fixed one for the test. That is the one
    // difference from production, where the daemon identity is generated on
    // first use and read back out of the store.
    // The control-plane key the peer signs with, and the client pins, from one
    // shared keypair -- there is no round trip to exchange one first.
    let (control_pubkey, control_seckey) = control_keypair();
    let peer = Peer::spawn(seed, chain_length, control_seckey);
    let config = LinkConfig {
        seed: hex_of(&seed),
        pinned_fingerprint: "fp-test".to_owned(),
        control_pubkey: hex_of(&control_pubkey),
        chain_length,
    };

    let mut link = node_link::connect(
        &config,
        "node-peer-test",
        &format!("http://127.0.0.1:{}", peer.port),
    )
    .expect("the client opens a session against the peer");
    // The call carries an epoch, which is what a node heartbeat sends. If the
    // envelope nests it, the peer reads null and this assertion is the one
    // that fails.
    let reply = link
        .call("node_heartbeat", serde_json::json!({ "epoch": 4242 }))
        .expect("the peer answers the request");
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["echo"], 4242);
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fixed control-plane keypair, shared by both halves.
///
/// The peer signs with it and the client pins it, so the two agree without a
/// round trip to exchange one first. A test key, never a shipped one.
fn control_keypair() -> (Vec<u8>, String) {
    let generated = handshake::generate_control_keypair().unwrap();
    let pubkey = box_node_link::handshake::hex_decode(&generated.pubkey_hex).unwrap();
    (pubkey, generated.seckey_hex)
}
