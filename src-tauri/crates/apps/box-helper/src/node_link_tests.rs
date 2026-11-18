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
            let payload = session
                .open_frame(&frame)
                .expect("the peer opens the request under the agreed keys");
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
        box_node_link::chain::ChainNode::new(seed, chain_length),
        std::net::TcpStream::connect(("127.0.0.1", peer.port)).unwrap(),
        None,
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

/// Decode hex to 32 bytes, for a chain value the walk hands out.
fn bytes_of(hex: &str) -> [u8; 32] {
    box_node_link::handshake::hex_decode(hex)
        .and_then(|v| v.try_into().ok())
        .expect("a chain value is 32 bytes of hex")
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


/// The client follows a rekey the peer sent with no request outstanding.
///
/// This is the case a proactive rotation produces: the daemon rotates when its
/// keys near their limit, so the frame arrives between requests and sits in the
/// socket buffer until the next one. A client that treats any non-response frame
/// as a refusal -- which is what `Link::call` did at first -- drops the link to
/// HTTP exactly when the daemon was trying to keep the session alive.
#[test]
fn the_client_follows_a_rekey_that_arrives_before_the_response() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, ControlHand};
    use std::io::{BufReader, BufWriter, Write};
    use std::net::TcpListener;

    let seed = [11u8; 32];
    let chain_length = 64u32;
    let (control_pubkey, control_seckey) = control_keypair();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = BufWriter::new(stream.try_clone().unwrap());
        let mut reader = BufReader::new(stream);

        let Some(frame) = read_frame(&mut reader).unwrap() else { return };
        assert_eq!(frame.frame_type, FrameType::Hello);
        let wire: box_node_link::wire::HelloWire =
            serde_json::from_slice(&frame.payload).unwrap();
        let hello = wire.to_hello().unwrap();

        let link = box_node_link::chain::hash_n(&seed, chain_length - 1);
        let signing = handshake::control_signing_key(&control_seckey).unwrap();
        let cert = serde_json::to_vec(&box_node_link::session::SessionCert {
            node_id: hello.node_id.clone(),
            control_fingerprint: "fp-test".to_owned(),
            issued_at: 1,
            expires_at: 2,
        })
        .unwrap();
        let control = ControlHand::new(&box_node_link::SystemRandom::new()).unwrap();
        let (keys, ack, _spent) = handshake::control_finish(
            control,
            &hello,
            handshake::fresh_nonce().unwrap(),
            &signing,
            &cert,
            &box_node_link::chain::hash_once(&link),
            None,
            false,
        )
        .expect("the peer accepts the handshake");

        let payload = serde_json::to_vec(&box_node_link::wire::ack_wire(&ack)).unwrap();
        write_frame(&mut writer, &Frame { frame_type: FrameType::HelloAck, payload }).unwrap();
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

        // The request. Opened under the pre-rotation keys, which is the pair
        // the peer sealed it with.
        let Some(frame) = read_frame(&mut reader).unwrap() else { return };
        let payload = session.open_frame(&frame).expect("the peer opens the request");
        let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(request["epoch"].as_u64(), Some(7));

        // The rekey FIRST, then the response. This is the ordering a
        // proactive rotation produces: the frame arrives with no request
        // outstanding, and a client that is not looking for it mistakes it
        // for a refusal.
        //
        // send_rekey, not seal-plus-recv: the peer is the sender here, and
        // send_rekey already installs the new pair on its own side. Calling
        // recv_rekey on a frame it just wrote would consume a second seq and
        // desynchronise the counters.
        session
            .send_rekey(&mut writer)
            .expect("the peer rotates and tells the client");

        // And the response, sealed under the NEW pair -- which is the whole
        // point: the client must have rotated to open it.
        let reply = serde_json::json!({ "ok": true, "after": "rekey" });
        let sealed = session
            .seal_frame(FrameType::Response, &serde_json::to_vec(&reply).unwrap())
            .unwrap();
        write_frame(&mut writer, &sealed).unwrap();
        writer.flush().unwrap();
    });

    let config = LinkConfig {
        seed: hex_of(&seed),
        pinned_fingerprint: "fp-test".to_owned(),
        control_pubkey: hex_of(&control_pubkey),
        chain_length,
    };
    let mut link = node_link::connect(
        &config,
        "node-follow-rekey",
        &format!("http://127.0.0.1:{port}"),
        box_node_link::chain::ChainNode::new(seed, chain_length),
        std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
        None,
    )
    .expect("the client opens a session");

    // One call. If the client does not follow the rekey, this fails at the
    // "the control plane refused" branch -- the rekey read as a refusal.
    let reply = link
        .call("node_heartbeat", serde_json::json!({ "epoch": 7 }))
        .expect("the client follows the rekey and reads the response");
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["after"], "rekey");
}


/// The walk steps down the chain, so a reconnect spends a fresh link.
///
/// This is the bug the whole change is about: the helper always sent
/// `c_(n-1)`, the control plane already held that value, and the second
/// handshake was refused -- so a link that dropped never came back and the node
/// heartbeated HTTP for the rest of its life while looking perfectly healthy.
///
/// The walk is tested directly rather than through a second connection,
/// because what has to be right is *which link* the next handshake offers, and
/// that is a property of the walk. A second daemon would only re-test the
/// handshake, which the first test already covers.
#[test]
fn the_walk_steps_down_so_a_reconnect_spends_a_fresh_link() {
    let seed = [13u8; 32];
    let chain_length = 64u32;
    let mut walker = box_node_link::chain::ChainNode::new(seed, chain_length);

    // The first handshake offers c_(n-1): the control plane holds c_n and
    // accepts a value only when H(value) equals it.
    let (first_hex, k) = walker.next_value();
    assert_eq!(k, chain_length - 1);
    let first = bytes_of(&first_hex);
    assert_eq!(first, box_node_link::chain::hash_n(&seed, chain_length - 1));

    // The second offers c_(n-2), not c_(n-1) again. This single assertion is
    // the difference between a link that reconnects and a link that is
    // refused forever.
    let (second_hex, k2) = walker.next_value();
    assert_eq!(k2, chain_length - 2);
    let second = bytes_of(&second_hex);
    assert_eq!(second, box_node_link::chain::hash_n(&seed, chain_length - 2));
    assert_ne!(first, second);

    // And the values really are preimages of each other, which is what the
    // control plane checks: H(c_k) is c_(k+1).
    assert_eq!(box_node_link::chain::hash_once(&second), first);
}

/// A walk that has reached the seed refuses to go further.
#[test]
fn an_exhausted_walk_says_so_rather_than_sending_the_seed() {
    let seed = [13u8; 32];
    // Two links is the smallest usable chain: c_1 is spendable, c_0 is the
    // seed.
    let mut walker = box_node_link::chain::ChainNode::new(seed, 2);
    let (first_hex, k) = walker.next_value();
    assert_eq!(k, 1);
    assert_ne!(bytes_of(&first_hex), seed, "the first value must not be the seed");

    // One link is left and it is the seed. The walk says so rather than
    // handing it out, because a seed on the wire is the one thing the node
    // must never do.
    assert!(walker.is_exhausted(), "a two-link chain is exhausted after one value");
}


/// A failed reconnect leaves the walk one step lower.
///
/// This is the repair for the design red line, and it was unpinned: deleting the
/// rewind_to call from reconnect left the whole suite green, which a reviewer
/// proved by mutation. The rewind is a recovery path whose failure mode is a
/// permanent refusal on the next attempt, so it deserves a test even though the
/// path that triggers it is hard to fake.
///
/// The bookkeeping is what is asserted, not the reconnect: the walk moves down
/// by one when an attempt fails, so the next attempt offers a link the control
/// plane has not already spent. That is the property, and it needs no socket.
#[test]
fn a_failed_reconnect_leaves_the_walk_one_step_lower() {
    let seed = [17u8; 32];
    let chain_length = 64u32;
    let mut walker = box_node_link::chain::ChainNode::new(seed, chain_length);

    // What the attempt will offer, captured the way reconnect captures it.
    let offered_k = walker.remaining().max(1);
    let (offered_hex, _) = walker.next_value();

    // The attempt fails. reconnect rewinds to one below what it offered, so
    // the next attempt does not re-offer a link the daemon may already hold.
    walker.rewind_to(offered_k.saturating_sub(1).max(1));

    let (retry_hex, retry_k) = walker.next_value();
    assert_eq!(retry_k, offered_k - 1, "the retry must offer the next link down");
    assert_ne!(retry_hex, offered_hex, "the retry must not re-offer the spent link");

    // And the two are preimages, which is what the control plane checks.
    assert_eq!(
        box_node_link::chain::hash_once(&bytes_of(&retry_hex)),
        bytes_of(&offered_hex),
        "H(c_k) is c_(k+1), so the retry is the preimage of what was offered"
    );
}

/// The rewind only ever moves the walk down, never back up.
#[test]
fn a_rewind_cannot_move_the_walk_back_up() {
    let seed = [17u8; 32];
    let mut walker = box_node_link::chain::ChainNode::new(seed, 64);
    walker.next_value();
    let after_first = walker.remaining();

    // A rewind to a HIGHER k than the current one must be ignored, or a
    // caller could push the walk back up to a link the daemon already spent.
    walker.rewind_to(63);
    assert_eq!(
        walker.remaining(),
        after_first,
        "a rewind above the current k must not move the walk"
    );
}


/// A reconnect that fails steps the walk back, through the real method.
///
/// rewind_after_failure is tested directly as a function, but that is not the
/// same as the call inside reconnect being tested -- the reviewer deleted that
/// call once and the suite stayed green. This one drives the method: a
/// successful handshake to build the Link, then a peer that refuses.
#[test]
fn a_failed_reconnect_steps_the_walk_back_through_the_method() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, ControlHand};
    use std::io::{BufReader, BufWriter, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let seed = [23u8; 32];
    let chain_length = 64u32;
    let (control_pubkey, control_seckey) = control_keypair();

    // A peer that completes ONE handshake and then refuses the next.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted2 = accepted.clone();
    let seckey2 = control_seckey.clone();
    std::thread::spawn(move || {
        loop {
            let incoming = listener.accept();
            let (stream, _) = match incoming {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            let mut reader = BufReader::new(stream);
            let frame = read_frame(&mut reader).unwrap();
            let frame = match frame {
                Some(frame) => frame,
                None => return,
            };
            assert_eq!(frame.frame_type, FrameType::Hello);

            let already = accepted2.fetch_add(1, Ordering::SeqCst);
            if already > 0 {
                let close = Frame::json(
                    FrameType::Close,
                    &serde_json::json!({ "reason": "handshake-refused" }),
                )
                .unwrap();
                write_frame(&mut writer, &close).unwrap();
                writer.flush().unwrap();
                return;
            }

            let wire: box_node_link::wire::HelloWire =
                serde_json::from_slice(&frame.payload).unwrap();
            let hello = wire.to_hello().unwrap();
            let offered = hello.link.expect("a token hello carries its link");
            let signing = handshake::control_signing_key(&seckey2).unwrap();
            let cert = serde_json::to_vec(&box_node_link::session::SessionCert {
                node_id: hello.node_id.clone(),
                control_fingerprint: "fp-test".to_owned(),
                issued_at: 1,
                expires_at: 2,
            })
            .unwrap();
            let control = ControlHand::new(&box_node_link::SystemRandom::new()).unwrap();
            let result = handshake::control_finish(
                control,
                &hello,
                handshake::fresh_nonce().unwrap(),
                &signing,
                &cert,
                &box_node_link::chain::hash_once(&offered),
                None,
                false,
            );
            let (keys, ack, _spent) = result.expect("the first handshake completes");

            let payload = serde_json::to_vec(&box_node_link::wire::ack_wire(&ack)).unwrap();
            let ack_frame = Frame {
                frame_type: FrameType::HelloAck,
                payload,
            };
            write_frame(&mut writer, &ack_frame).unwrap();
            writer.flush().unwrap();
            let _ = keys;
        }
    });

    let config = LinkConfig {
        seed: hex_of(&seed),
        pinned_fingerprint: "fp-test".to_owned(),
        control_pubkey: hex_of(&control_pubkey),
        chain_length,
    };
    let started_at = box_node_link::chain::ChainNode::new(seed, chain_length).remaining();

    let mut link = node_link::connect(
        &config,
        "node-method-rewind",
        &format!("http://127.0.0.1:{port}"),
        box_node_link::chain::ChainNode::new(seed, chain_length),
        std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
        None,
    )
    .expect("the first handshake completes");

    assert!(link.walk_k() < started_at, "the first handshake spent a link");

    let before_retry = link.walk_k();
    let error = link
        .reconnect()
        .expect_err("the refusing peer must fail the reconnect");
    assert!(error.contains("refused"), "the refusal reason travels: {error}");
    assert_eq!(
        link.walk_k(),
        before_retry - 1,
        "a failed reconnect must leave the walk one step lower"
    );
}



/// A restart reconnects: the resumed link is one the control plane accepts.
///
/// The first version of this test asserted that resuming AT the persisted k
/// gives that k back -- which is the BUG, not the fix. The file holds the link
/// that was spent; resuming at it re-offers the value the control plane already
/// holds, and it is refused forever. The reviewer proved that with the daemon
/// own acceptance check, so this test drives the real resume path and
/// cross-checks the result against `chain::accept`.
///
/// It goes through `resume_walk`, not through its own arithmetic: the decision
/// lives between two files, and a decision in the gap is a decision nothing
/// checks. A version that recomputed the subtraction locally passed even with
/// the bug restored, which is how this was found the second time.
#[test]
fn a_restart_resumes_at_a_link_the_control_plane_accepts() {
    let seed = [31u8; 32];
    let chain_length = 64u32;
    let dir = std::env::temp_dir().join(format!("dshbox-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // The first process: walk, spend a link, persist the position.
    let mut walker = box_node_link::chain::ChainNode::new(seed, chain_length);
    let (spent_hex, spent_k) = walker.next_value();
    node_link::write_walk(&dir, spent_k).expect("the position is written");

    // The daemon accepted it, so it now holds that value and expects its
    // preimage. This is the daemon own check, not a reimplementation of it.
    let mut state = box_node_link::chain::ChainState::from_seed(&seed, chain_length);
    assert!(box_node_link::chain::accept(&mut state, &spent_hex).is_ok());

    // The second process, through the real resume path.
    let mut resumed = node_link::resume_walk(seed, chain_length, Some(&dir));
    let (offered_hex, offered_k) = resumed.next_value();

    assert_ne!(offered_hex, spent_hex, "a restart must not re-offer the spent link");
    assert!(offered_k < spent_k, "the resumed walk must be below the spent link");

    // And the daemon accepts it, which is the whole claim checked against the
    // daemon own rule rather than against my opinion of it.
    let verdict = box_node_link::chain::accept(&mut state, &offered_hex);
    assert!(verdict.is_ok(), "the resumed link must be accepted: {verdict:?}");

    // And the BUG is refused, so this test cannot be satisfied by the old
    // behaviour. Re-offering the spent link is what the first version did.
    assert!(box_node_link::chain::accept(&mut state, &spent_hex).is_err());

    // No state directory means a fresh walk, which is stage A behaviour.
    let fresh = node_link::resume_walk(seed, chain_length, None);
    assert_eq!(fresh.remaining(), chain_length - 1, "no state means a fresh walk");

    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn a_write_leaves_no_temp_file() {
    let dir = std::env::temp_dir().join(format!("dshbox-walk-tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    node_link::write_walk(&dir, 40).expect("the write succeeds");
    let mut leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    leftovers.sort();
    assert_eq!(leftovers, vec![String::from("node-link-walk.json")], "only the state file remains");

    let _ = std::fs::remove_dir_all(&dir);
}

/// ChainNode::at resumes at a specific k, and clamps one that is out of range.
#[test]
fn a_resumed_walk_clamps_an_out_of_range_k() {
    let seed = [3u8; 32];
    // A k beyond the chain means the chain length changed under us. Clamping
    // keeps the walk inside the chain rather than refusing to start.
    let high = box_node_link::chain::ChainNode::at(seed, 999, 64);
    assert!(high.remaining() <= 63, "a k past the end is clamped into the chain");

    // A k of zero would be the seed itself, which must never be offered.
    let mut low = box_node_link::chain::ChainNode::at(seed, 0, 64);
    let (_hex, k) = low.next_value();
    assert!(k >= 1, "a k of zero is clamped to one, never the seed");
}


/// The walk is persisted before the hello reaches the peer.
///
/// The whole feature is this ordering, and a reorder mutation proved nothing
/// pinned it: moving the write after the flush left the entire suite green. So
/// the peer checks the file at the moment it sees the hello -- which is the
/// only place the ordering is observable from outside the process.
#[test]
fn the_walk_is_persisted_before_the_hello_reaches_the_peer() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    // The peer completes no handshake: it only has to be there.
    use std::io::{BufReader, BufWriter, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    let seed = [41u8; 32];
    let chain_length = 64u32;
    let (control_pubkey, _seckey) = control_keypair();
    let dir = std::env::temp_dir().join(format!("dshbox-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // The peer reports what it saw, and the test asserts on it.
    let (seen_tx, seen_rx) = mpsc::channel();
    let dir_for_peer = dir.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let incoming = listener.accept();
        let (stream, _) = match incoming {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = BufWriter::new(stream);
        // The hello itself is not inspected -- only its timing. A peer that
        // reads it has already received the bytes the client flushed.
        let _frame = read_frame(&mut reader).unwrap();
        // At the moment the hello arrives, the state file must already exist.
        // A write that happens after the flush cannot satisfy this.
        let existed = node_link::read_walk(&dir_for_peer);
        let _ = seen_tx.send(existed);

        // Then refuse, so the client does not hang waiting for an ack.
        let close = Frame::json(
            FrameType::Close,
            &serde_json::json!({ "reason": "test-refusal" }),
        )
        .unwrap();
        write_frame(&mut writer, &close).unwrap();
        writer.flush().unwrap();
        // Drop the writer, so the socket closes and the client read of the
        // ack ends rather than waiting for bytes that are never coming.
        drop(writer);
        drop(reader);
    });

    let config = LinkConfig {
        seed: hex_of(&seed),
        pinned_fingerprint: "fp-test".to_owned(),
        control_pubkey: hex_of(&control_pubkey),
        chain_length,
    };
    let walker = box_node_link::chain::ChainNode::new(seed, chain_length);
    let _ = node_link::connect(
        &config,
        "node-order",
        &format!("http://127.0.0.1:{port}"),
        walker,
        std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
        Some(&dir),
    );

    // The peer saw the file already written when the hello arrived.
    let seen = seen_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(seen.is_ok(), "the peer must have observed something");
    assert_eq!(
        seen.unwrap(),
        Some(chain_length - 1),
        "the walk must be on disk before the hello is on the wire"
    );

    let _ = std::fs::remove_dir_all(&dir);
}


/// A write that fails stops the handshake, rather than letting the hello out.
///
/// The second blocker in the first review of this PR was exactly this: the
/// error was logged and ignored, so the link was spent on the wire with nothing
/// on disk -- the scenario the write exists to prevent. The fix returns Err, but
/// the re-review proved the error *propagation* was unpinned: swallowing it left
/// the committed suite green, because the ordering test fails only on the file
/// being absent, not on an error being ignored.
///
/// So the test hands `connect` a state path that cannot be created -- a regular
/// file where a directory is needed -- and asserts the error names the
/// persistence. If the error were swallowed the hello would go out and the peer
/// would answer it, which is the other assertion here.
#[test]
fn a_write_that_fails_stops_the_handshake_rather_than_letting_the_hello_out() {
    use box_node_link::frame::{read_frame, FrameType};
    use std::io::{BufReader, BufWriter, Write};
    use std::net::TcpListener;

    let seed = [53u8; 32];
    let chain_length = 64u32;
    let (control_pubkey, _seckey) = control_keypair();

    // A peer that reports whether a hello arrived at all.
    let (hello_tx, hello_rx) = std::sync::mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let incoming = listener.accept();
        let (stream, _) = match incoming {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = BufWriter::new(stream);
        let frame = read_frame(&mut reader).unwrap();
        let _ = hello_tx.send(frame.map(|f| f.frame_type));
        let close = box_node_link::frame::Frame::json(
            FrameType::Close,
            &serde_json::json!({ "reason": "unreachable" }),
        )
        .unwrap();
        box_node_link::frame::write_frame(&mut writer, &close).unwrap();
        writer.flush().unwrap();
    });

    // A path that exists as a FILE, so create_dir_all on it must fail. That is
    // the shape of a full or read-only filesystem without needing one.
    let blocker = std::env::temp_dir().join(format!("dshbox-nodir-{}", std::process::id()));
    let _ = std::fs::remove_file(&blocker);
    std::fs::write(&blocker, "not a directory").unwrap();

    let config = LinkConfig {
        seed: hex_of(&seed),
        pinned_fingerprint: "fp-test".to_owned(),
        control_pubkey: hex_of(&control_pubkey),
        chain_length,
    };
    let walker = box_node_link::chain::ChainNode::new(seed, chain_length);
    let result = node_link::connect(
        &config,
        "node-write-fails",
        &format!("http://127.0.0.1:{port}"),
        walker,
        std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
        Some(&blocker),
    );

    // The error names the persistence, not the handshake. A swallowed error
    // would let the hello out and the failure would read as a refusal instead.
    let error = match result {
        Ok(_) => panic!("a write that fails must stop the handshake"),
        Err(error) => error,
    };
    assert!(
        error.contains("cannot persist the chain walk"),
        "the reason must be the persistence: {error}"
    );

    // And no hello reached the peer, which is the point of stopping. The peer
    // does report something -- a clean EOF, because the socket closed without
    // a frame -- but the frame type it saw is None rather than Hello.
    let seen = hello_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert_eq!(seen.ok(), Some(None), "no hello frame reached the peer");

    let _ = std::fs::remove_file(&blocker);
}
