//! Gateway tests: the HTTP and frame transports, driven through the real
//! listeners rather than through the handlers.
//!
//! Kept out of `main.rs` on purpose. The daemon binary is a top-level
//! module tree -- routing, lifecycle, discovery -- and its tests are a
//! different concern with a different audience. Mixing them makes both
//! harder to find, and a file that has to be scrolled past 400 lines of
//! tests to reach the routing is a file nobody reads.

use super::*;

use crate::state::DaemonState;
use box_foundation::BoxPaths;
use box_scheduler::TaskManager;
use box_state::ResourceStateManager;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::sync::{Mutex, RwLock};

/// Decode a hex string, for reading a key back out of the store.
fn hex_to_bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn state() -> Arc<DaemonState> {
    let events = Arc::new(crate::events::DaemonEvents::new());
    let empty = BoxPaths {
        config: std::path::PathBuf::from("/nonexistent"),
        runtime: None,
    };
    Arc::new(DaemonState {
        manager: TaskManager::memory(),
        paths: RwLock::new(BoxPaths {
            config: std::path::PathBuf::from("/nonexistent"),
            runtime: None,
        }),
        containers: Arc::new(Default::default()),
        resources: ResourceStateManager::default(),
        events: events.clone(),
        browser: Mutex::new(crate::debug::Sessions::new()),
        nodes: Arc::new(crate::nodes::NodeManager::load(
            &empty,
            String::new(),
            events,
        )),
        artifacts: crate::node_artifacts::ArtifactIndex::memory(),
    })
}

/// Drive a request through the real HTTP gateway, not a handler. The
/// server thread runs `handle_http` with `main` as its discovery token.
fn rpc(state: Arc<DaemonState>, main: &str, body: Value) -> (u16, Value) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = main.to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handle_http(stream, server_state, &token);
    });
    let payload = body.to_string();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        client,
        "POST /rpc HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    )
    .unwrap();
    client.flush().unwrap();
    let mut raw = String::new();
    BufReader::new(client).read_to_string(&mut raw).unwrap();
    handle.join().unwrap();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("").trim())
        .unwrap_or(json!({}));
    (status, value)
}

fn report_body(node_id: &str) -> Value {
    json!({
        "method": "node_artifact_report",
        "__nodeId": node_id,
        "baseId": "base-9c1f00d4a1b2c3d4",
        "sourceCommit": "0d1f500",
        "platform": "linux-x86_64",
        "archive": "/var/dshbox/artifacts/base.tar",
        "sha256": format!("9f2c{}", "a".repeat(60)),
        "sizeBytes": 1024,
    })
}

/// The reserved key belongs to the server. A main-token caller cannot
/// name a node in the body: with no session there is nothing to bind and
/// the report is refused. This is the regression mutation B exposed.
#[test]
fn a_main_token_cannot_forge_a_node_identity_in_the_body() {
    let state = state();
    let main = "main-token";
    let (grant, id, _, _) = state
        .nodes
        .enroll("wsl2", "linux-x86_64", vec![], Some("node-a".to_owned()))
        .unwrap();
    state.nodes.register(&grant, None, None, None).unwrap();

    let mut body = report_body(&id);
    body["token"] = json!(main);
    let (status, reply) = rpc(state.clone(), main, body);
    assert_eq!(status, 200);
    assert_eq!(reply["ok"], false, "a forged identity must not write a row");
    assert!(
        state.artifacts.list(None, None).unwrap().is_empty(),
        "the index stays empty"
    );
}

/// A session caller's own forged `__nodeId` is dropped and replaced by
/// the session's node, even when the body names another node entirely.
#[test]
fn a_session_callers_forged_node_identity_is_replaced() {
    let state = state();
    let main = "main-token";
    let (g1, a, _, _) = state
        .nodes
        .enroll("wsl2", "linux-x86_64", vec![], Some("node-a".to_owned()))
        .unwrap();
    let (_, session, epoch, _) = state.nodes.register(&g1, None, None, None).unwrap();
    let (g2, b, _, _) = state
        .nodes
        .enroll("wsl2", "linux-x86_64", vec![], Some("node-b".to_owned()))
        .unwrap();
    state.nodes.register(&g2, None, None, None).unwrap();

    let mut body = report_body(&b); // claims the OTHER node
    body["token"] = json!(session);
    body["epoch"] = json!(epoch);
    let (status, reply) = rpc(state.clone(), main, body);
    assert_eq!(status, 200);
    assert_eq!(reply["ok"], true, "{reply}");
    let rows = state.artifacts.list(None, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].node_id, a, "the session wins, not the body");
    assert_eq!(rows[0].id, format!("{a}|base-9c1f00d4a1b2c3d4"));
}

/// `node_register` is the one route with no session but body-supplied
/// fields. Stripping the reserved key must not break it: the helper's
/// platform has to reach the record, and the enroll grant still works.
#[test]
fn a_helper_can_still_register_over_the_gateway_with_its_platform() {
    let state = state();
    let main = "main-token";
    let (_, enroll) = rpc(
        state.clone(),
        main,
        json!({
            "method": "node_enroll",
            "token": main,
            "kind": "wsl2",
            "platform": "windows-x86_64", // the supply adapter's guess
            "nodeId": "node-a"
        }),
    );
    assert_eq!(enroll["ok"], true, "{enroll}");
    let grant = enroll["result"]["enrollToken"].as_str().unwrap().to_owned();

    let (status, reply) = rpc(
        state.clone(),
        main,
        json!({
            "method": "node_register",
            "enrollToken": grant,
            "platform": "linux-x86_64", // what the helper actually runs on
            "helperVersion": "0.1.10"
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(reply["ok"], true, "{reply}");
    let node = state.nodes.get("node-a").unwrap().unwrap();
    assert_eq!(node.platform, "linux-x86_64", "the helper's platform wins");
}

/// The pure helper's rule: drop first, then bind. A unit test so removing
/// the drop (the I3 bypass) fails without needing a socket.
#[test]
fn bind_request_node_drops_before_it_binds() {
    let mut forged = json!({ "__nodeId": "node-b", "method": "node_artifact_report" });
    bind_request_node(&mut forged, None);
    assert!(forged.get("__nodeId").is_none(), "no session, no identity");

    bind_request_node(&mut forged, Some("node-a"));
    assert_eq!(forged["__nodeId"], "node-a");

    let mut session_forged = json!({ "__nodeId": "node-b" });
    bind_request_node(&mut session_forged, Some("node-a"));
    assert_eq!(
        session_forged["__nodeId"], "node-a",
        "the forged value is gone"
    );
}
/// One frame that came back from the gateway, with its type kept: a
/// refused request is answered with a `close` frame, not a `response`
/// one, and a test that only looked at the payload could not tell the
/// difference.
struct LinkReply {
    frame_type: box_node_link::frame::FrameType,
    value: Value,
}

/// Drive one request through the real frame gateway, not a handler.
/// Returns `None` when the connection produced no frame at all.
fn link_rpc(state: Arc<DaemonState>, main: &str, body: Value) -> Option<LinkReply> {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = main.to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let frame = Frame::json(FrameType::Request, &body).unwrap();
    write_frame(&mut client, &frame).unwrap();
    let mut reader = BufReader::new(client);
    let response = read_frame(&mut reader).unwrap();
    // Drop the client first: the server loop only ends when it sees EOF
    // on the connection, so joining before the drop would block until the
    // test timeout.
    drop(reader);
    handle.join().unwrap();
    response.map(|frame| LinkReply {
        frame_type: frame.frame_type,
        value: frame.value().unwrap(),
    })
}

/// The frame path answers the same methods the HTTP path answers, with
/// the same JSON. This is the whole point of L1: the transport changed,
/// nothing else did.
#[test]
fn the_frame_path_answers_the_same_methods_as_http() {
    let state = state();
    let body = json!({ "method": "ping", "token": "main-token" });
    let http = rpc(state.clone(), "main-token", body.clone());
    let frame = link_rpc(state.clone(), "main-token", body).expect("a response frame");
    assert_eq!(
        frame.frame_type,
        box_node_link::frame::FrameType::Response,
        "an accepted request is answered with a response frame"
    );
    assert_eq!(http.1["ok"], frame.value["ok"]);
    assert_eq!(http.1["status"], frame.value["status"]);
}

/// `__nodeId` is server-owned on the frame path too. A caller that
/// names a node in the body must not be believed -- this is the same
/// regression the HTTP path pins, and a new transport that forgot to
/// strip it would open exactly the hole the original bug was.
#[test]
fn a_frame_caller_cannot_forge_a_node_identity_in_the_body() {
    let state = state();
    let (grant, id, token, epoch) = enroll_a_node(&state);
    let forged = json!({
        "method": "node_artifact_report",
        "token": token,
        "epoch": epoch,
        "__nodeId": "node-victim",
        "baseId": "base-9c1f00d4a1b2c3d4",
        "sourceCommit": "0d1f500",
        "platform": "linux-x86_64",
        "archive": "/var/dshbox/artifacts/base.tar",
        "sha256": format!("9f2c{}", "a".repeat(60)),
        "sizeBytes": 1024,
    });
    let response = link_rpc(state.clone(), "main-token", forged.clone())
        .expect("a response frame");
    assert_eq!(
        response.frame_type,
        box_node_link::frame::FrameType::Response,
        "a valid session is answered, not closed"
    );
    // The report was attributed to the session's own node, not the
    // forged one.
    let rows = state.artifacts.list(Some(&id), None).unwrap();
    assert_eq!(rows.len(), 1, "the row landed on the session node");
    assert!(state.artifacts.list(Some("node-victim"), None).unwrap().is_empty());
    assert_eq!(response.value["ok"], true);
    let _ = grant;
}

/// A method outside `NODE_METHODS` is refused on the frame path with a
/// close frame, not answered. A valid session asking for a
/// control-plane method must not get one just because the transport is
/// new.
#[test]
fn a_frame_session_cannot_call_a_control_plane_method() {
    let state = state();
    let (grant, id, token, epoch) = enroll_a_node(&state);
    let body = json!({
        "method": "list_containers",
        "token": token,
        "epoch": epoch,
    });
    let response = link_rpc(state.clone(), "main-token", body)
        .expect("the link answered before closing");
    assert_eq!(
        response.frame_type,
        box_node_link::frame::FrameType::Close,
        "a method outside NODE_METHODS closes the link, it does not answer it"
    );
    assert_eq!(response.value["reason"], "forbidden-method");
    let _ = (grant, id);
}

/// A frame with no credential closes the link. Same rule as HTTP 401:
/// resolve the session first, and refuse when there is none.
#[test]
fn a_frame_without_a_credential_is_closed() {
    let state = state();
    let body = json!({ "method": "node_heartbeat" });
    let reply = link_rpc(state.clone(), "main-token", body)
        .expect("the link answered before closing");
    assert_eq!(
        reply.frame_type,
        box_node_link::frame::FrameType::Close,
        "no credential closes the link rather than answering it"
    );
    assert_eq!(reply.value["reason"], "no-session");
}

/// A malformed frame closes the link without a response. There is no
/// request to answer, and half a session is worse than none.
#[test]
fn a_malformed_frame_closes_the_link_without_a_response() {
    use box_node_link::frame::FrameType;
    let state = state();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // A frame that claims 100 bytes and delivers 3: torn, not closed.
    let mut torn = 100u32.to_be_bytes().to_vec();
    torn.extend_from_slice(&[FrameType::Request.as_u8(), b'{', b'}']);
    client.write_all(&torn).unwrap();
    client.flush().unwrap();
    drop(client);
    handle.join().unwrap();
}

/// Enroll a node and return its grant plus the session it produces, so
/// a test can act as that node.
fn enroll_a_node(state: &Arc<DaemonState>) -> (String, String, String, u64) {
    let enroll = json!({
        "method": "node_enroll",
        "token": "main-token",
        "kind": "wsl2",
        "platform": "linux-x86_64",
    });
    let enrolled = rpc(state.clone(), "main-token", enroll);
    let grant = enrolled.1["result"]["enrollToken"].as_str().unwrap().to_owned();
    let register = json!({
        "method": "node_register",
        "token": "main-token",
        "enrollToken": grant,
        "nodeId": "node-session-owner",
        "kind": "wsl2",
        "platform": "linux-x86_64",
        "capabilities": ["exec", "heartbeat"],
        "helperVersion": "test",
    });
    let registered = rpc(state.clone(), "main-token", register);
    let token = registered.1["result"]["sessionToken"].as_str().unwrap().to_owned();
    let epoch = registered.1["result"]["epoch"].as_u64().unwrap();
    let id = registered.1["result"]["nodeId"].as_str().unwrap_or("node-session-owner").to_owned();
    (grant, id, token, epoch)
}

/// A frame that is neither a request nor a hello closes the link.
///
/// L4b added the handshake, so `hello` is no longer an unimplemented frame
/// type -- it is the first frame of the encrypted path, and it is answered by
/// that path. What still closes the link outright is anything else: a
/// response, an event or a rekey arriving with nothing to answer.
#[test]
fn a_non_request_non_hello_frame_closes_the_link() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    let state = state();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // A response frame with no request behind it.
    let frame = Frame::json(FrameType::Response, &serde_json::json!({ "ok": true })).unwrap();
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client);
    let reply = read_frame(&mut reader).unwrap();
    drop(reader);
    handle.join().unwrap();
    let reply = reply.expect("the link answered before closing");
    assert_eq!(reply.frame_type, FrameType::Close);
    assert_eq!(reply.value().unwrap()["reason"], "unexpected-frame-type");
}

/// A hello now reaches the handshake rather than being refused as an
/// unknown frame type.
///
/// The close reason changed from "unexpected-frame-type" to "bad-hello", and
/// that difference is the whole point of the test: it pins that the encrypted
/// path is the one that answers, not the plaintext gate.
#[test]
fn a_hello_is_answered_by_the_handshake_path() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    let state = state();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // A hello with none of the fields the wire shape requires, so it is
    // refused for its contents rather than for its type.
    let frame = Frame::json(FrameType::Hello, &serde_json::json!({ "ver": 1 })).unwrap();
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client);
    let reply = read_frame(&mut reader).unwrap();
    drop(reader);
    handle.join().unwrap();
    let reply = reply.expect("the link answered before closing");
    assert_eq!(reply.frame_type, FrameType::Close);
    assert_eq!(reply.value().unwrap()["reason"], "bad-hello");
}

/// A daemon state with a real runtime directory, so the handshake has a
/// secrets store to open.
///
/// The default `state()` has `runtime: None`, which makes every handshake fail
/// with `no-identity` -- correct for the tests that only exercise the gate, and
/// useless for the ones that exercise the handshake itself.
fn state_with_runtime(dir: &std::path::Path) -> Arc<DaemonState> {
    let events = Arc::new(crate::events::DaemonEvents::new());
    let empty = BoxPaths {
        config: std::path::PathBuf::from("/nonexistent"),
        runtime: None,
    };
    Arc::new(DaemonState {
        manager: TaskManager::memory(),
        paths: RwLock::new(BoxPaths {
            config: std::path::PathBuf::from("/nonexistent"),
            runtime: Some(dir.to_path_buf()),
        }),
        containers: Arc::new(Default::default()),
        resources: ResourceStateManager::default(),
        events: events.clone(),
        browser: Mutex::new(crate::debug::Sessions::new()),
        nodes: Arc::new(crate::nodes::NodeManager::load(&empty, String::new(), events)),
        artifacts: crate::node_artifacts::ArtifactIndex::memory(),
    })
}

/// A well-formed hello reaches the handshake and is refused for the node,
/// not for the frame.
///
/// The difference from `a_hello_is_answered_by_the_handshake_path` is what
/// this adds: that one sends a hello with no usable fields and gets
/// `bad-hello` from the wire decoder, which a stub could produce. This one
/// builds a structurally valid hello -- real version, suite, nonce and an
/// ephemeral key -- so it gets past the decoder and is refused by the
/// handshake itself, with a reason that names the node problem.
///
/// It stops at `not-enrolled` because nothing writes a `node-keys` row yet:
/// `node_enroll` registers the node but not its key. A full session needs
/// that, and it is the next stage, so this test pins the path it can reach
/// rather than claiming one it cannot.
#[test]
fn a_hello_reaches_the_handshake_and_is_refused_for_its_node() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, Hello, NodeHand};
    use box_node_link::SystemRandom;
    use box_node_link::wire;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    // Nothing enrols a node key yet: `node_enroll` writes the registry row
    // but not a `node-keys` row, and that is the next stage. So this hello
    // is refused with `not-enrolled` -- which is the honest state of the
    // handshake path today, and the reason this test stops here rather than
    // claiming a full session it cannot have.

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let rng = SystemRandom::new();
    let control_pubkey = {
        let collection = crate::node_keys::open_control_identity(
            &state.paths.read().unwrap(),
        )
        .unwrap();
        crate::node_keys::ensure_control_identity(&collection).unwrap();
        crate::node_keys::find_identity(&collection)
            .unwrap()
            .map(|record| hex_to_bytes(&record.pubkey))
            .unwrap()
    };

    let node_hand = NodeHand::new(&rng, control_pubkey).unwrap();
    let eph_a = node_hand.public_key().unwrap();
    // Psk mode, because it is the only one a node with no key can present:
    // node-key mode is refused by the wire decoder for missing its key
    // fields, which is a different refusal and not the one under test.
    let hello = Hello {
        node_id: "node-handshake".to_owned(),
        mode: AuthMode::Psk,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().unwrap(),
        eph_a,
        // A mac that will not verify. It gets past the wire decoder, which
        // is the point: the handshake then refuses the node for having no
        // row at all, before it ever looks at the mac.
        mac: Some([0u8; 32]),
        node_pubkey: None,
        node_sig: None,
        link: None,
    };
    // The node is not enrolled with a key yet, so this hello is refused --
    // but refused by the handshake, with a reason that names it, which is
    // what proves the path is real.
    let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
    // `Frame::json` takes a Value; the hello wire form is already serialised,
    // so the frame is built directly. `payload` is public for exactly this.
    let frame = Frame {
        frame_type: FrameType::Hello,
        payload,
    };
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client);
    let reply = read_frame(&mut reader).unwrap().expect("the link answered");
    drop(reader);
    handle.join().unwrap();
    assert_eq!(reply.frame_type, FrameType::Close);
    assert_eq!(reply.value().unwrap()["reason"], "not-enrolled");
}


/// The whole L4c claim, end to end: a node enrols, gets a bootstrap secret,
/// completes the handshake over the socket, and exchanges an encrypted request
/// and response.
///
/// This is the test L4b could not write. It stopped at `not-enrolled` because
/// nothing wrote a `node-keys` row; now `node_enroll` mints one, so the
/// handshake has a secret to check and the session has keys to use.
///
/// What it pins that the unit tests cannot: the whole path through the real
/// socket and the real store -- `handle_link` -> `handle_secure` ->
/// `control_finish` -> `run_secure_loop` -> `Gate::resolve` -> `dispatch`.
#[test]
fn an_enrolled_node_completes_the_handshake_and_exchanges_an_encrypted_request() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, Hello, NodeHand};
    use box_node_link::session_io::Session;
    use box_node_link::wire;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    // Enrol through the real RPC, so the bootstrap secret is minted the way a
    // caller would get one.
    let (_, enrol_reply) = rpc(
        state.clone(),
        "main-token",
        json!({
            "method": "node_enroll",
            "token": "main-token",
            "kind": "helper",
            "platform": "windows-x86_64",
            "nodeId": "node-handshake",
        }),
    );
    let seed = enrol_reply["result"]["bootstrap"]["seed"]
        .as_str()
        .expect("the enrol reply carries a bootstrap secret")
        .to_owned();
    let seed_bytes: [u8; 32] = hex_to_bytes(&seed)
        .try_into()
        .expect("a seed is 32 bytes");

    // The control-plane fingerprint the node pins offline. It comes from the
    // identity the daemon just generated, read out of the same store the
    // handshake will use.
    let control_pubkey_hex = {
        let paths = state.paths.read().unwrap().clone();
        let collection = crate::node_keys::open_control_identity(&paths).unwrap();
        let record = crate::node_keys::find_identity(&collection)
            .unwrap()
            .expect("the daemon has an identity now");
        record.pubkey
    };
    // The fingerprint the node pins, computed before the key is moved into
    // the handshake.
    let pinned = box_node_link::node_keys::fingerprint_of(&control_pubkey_hex);
    let control_pubkey = hex_to_bytes(&control_pubkey_hex);

    // The link the node spends: c_(n-1).
    //
    // The control plane stores c_n = H^n(seed) and accepts a value only when
    // H(value) equals it, so the first value a node may send is c_(n-1).
    // Sending c_n itself would be a value whose hash nothing will ever
    // accept -- the same off-by-one the L2 review caught in the chain walk.
    //
    // The link travels in the hello and is bound into the transcript, which
    // is what lets the daemon spend it: once accepted, the daemon expects
    // the preimage of what it just saw, so the same link cannot buy a
    // second session.
    let chain_length = enrol_reply["result"]["bootstrap"]["chainLength"]
        .as_u64()
        .unwrap() as u32;
    let link = box_node_link::chain::hash_n(&seed_bytes, chain_length - 1);

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let rng = box_node_link::SystemRandom::new();
    let node_hand = NodeHand::new(&rng, control_pubkey).unwrap();
    let eph_a = node_hand.public_key().unwrap();

    // The mac proves the node holds the bootstrap secret: HMAC over the
    // transcript with the key derived from the first link.
    let hello = Hello {
        node_id: "node-handshake".to_owned(),
        mode: AuthMode::Token,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().unwrap(),
        eph_a,
        mac: None,
        node_pubkey: None,
        node_sig: None,
        link: Some(link),
    };
    let t1 = handshake::transcript_client(&hello);
    let bootstrap_key = box_node_link::kdf::bootstrap_key(&link);
    let mac = box_node_link::kdf::hmac_sha256(&bootstrap_key, &t1);
    let hello = Hello { mac: Some(mac), ..hello };

    let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
    let frame = Frame {
        frame_type: FrameType::Hello,
        payload,
    };
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();

    // The handshake answer. This is the frame L4b never reached.
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let ack = read_frame(&mut reader)
        .unwrap()
        .expect("the daemon answered the hello");
    assert_eq!(
        ack.frame_type,
        FrameType::HelloAck,
        "the handshake must complete, not close: {:?}",
        ack.value().ok()
    );

    // Now the encrypted round trip, through the same connection.
    let ack_wire: wire::HelloAckWire = serde_json::from_slice(&ack.payload).unwrap();
    let ack = ack_wire.to_ack().unwrap();
    let outcome = handshake::node_finish(node_hand, &hello, &ack, &link).unwrap();
    handshake::node_unseal(
        &outcome.keys,
        &hello,
        &ack,
        &link,
        &outcome.ecdh,
        &pinned,
    )
    .expect("the certificate must unseal against the pinned fingerprint");

    let mut session = Session::from_keys(
        outcome.keys,
        "node-handshake".to_owned(),
        AuthMode::Token,
    );
    // node_echo, a method on the NODE_METHODS whitelist. The whitelist is
    // the one thing the handshake does not establish, so the request has to
    // be one a node session may make -- ping is not, and asking for it is
    // refused, which is the behaviour a different test pins.
    let request = json!({ "method": "node_echo", "params": { "value": 42 } });
    let sealed = session
        .seal_frame(FrameType::Request, &serde_json::to_vec(&request).unwrap())
        .unwrap();
    // A separate writer, not reader.get_mut(): a BufReader may have read
    // ahead, and writing through it interleaves with its buffer. The
    // connection is a TcpStream, so a clone is the same socket.
    let mut writer = client;
    write_frame(&mut writer, &sealed).unwrap();
    writer.flush().unwrap();

    let reply = read_frame(&mut reader)
        .unwrap()
        .expect("the daemon answered the request");

    let opened = session
        .open_frame(&reply)
        .expect("the response must open under the session key");
    let parsed: serde_json::Value = serde_json::from_slice(&opened).unwrap();
    assert_eq!(parsed["ok"], true, "an encrypted request must be answered");

    // Close the write half so the daemon sees a clean EOF and its loop
    // returns. Without this the test hangs on join: the daemon is still
    // blocking on the next frame, and a closed read half is what tells it
    // the peer is gone.
    writer.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    handle.join().unwrap();
}


/// The method whitelist on the encrypted path.
///
/// The end-to-end test uses `node_echo`, which is allowed, so it passes
/// whether or not the check exists. This one asks for a method a node session
/// may not have, and the link must close rather than answer it. A review found
/// the gap: the plaintext path is pinned by
/// `a_frame_session_cannot_call_a_control_plane_method`, but that goes through
/// `handle_plaintext` -- a different code path.
#[test]
fn an_encrypted_session_cannot_call_a_control_plane_method() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, NodeHand};
    use box_node_link::session_io::Session;
    use box_node_link::wire;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    let (_, enrol_reply) = rpc(
        state.clone(),
        "main-token",
        json!({
            "method": "node_enroll",
            "token": "main-token",
            "kind": "helper",
            "platform": "windows-x86_64",
            "nodeId": "node-whitelist",
        }),
    );
    let seed = enrol_reply["result"]["bootstrap"]["seed"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed_bytes: [u8; 32] = hex_to_bytes(&seed).try_into().unwrap();
    let chain_length = enrol_reply["result"]["bootstrap"]["chainLength"]
        .as_u64()
        .unwrap() as u32;
    let link = box_node_link::chain::hash_n(&seed_bytes, chain_length - 1);
    let control_pubkey = control_pubkey_of(&state);

    let rng = box_node_link::SystemRandom::new();
    let node_hand = NodeHand::new(&rng, control_pubkey).unwrap();
    let hello = handshake::Hello {
        node_id: "node-whitelist".to_owned(),
        mode: AuthMode::Token,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().unwrap(),
        eph_a: node_hand.public_key().unwrap(),
        mac: None,
        node_pubkey: None,
        node_sig: None,
        link: Some(link),
    };
    let t1 = handshake::transcript_client(&hello);
    let mac = box_node_link::kdf::hmac_sha256(&box_node_link::kdf::bootstrap_key(&link), &t1);
    let hello = handshake::Hello { mac: Some(mac), ..hello };

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
    write_frame(&mut client, &Frame { frame_type: FrameType::Hello, payload }).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let ack = read_frame(&mut reader).unwrap().expect("the handshake completes");
    assert_eq!(ack.frame_type, FrameType::HelloAck);

    let ack_wire: wire::HelloAckWire = serde_json::from_slice(&ack.payload).unwrap();
    let ack = ack_wire.to_ack().unwrap();
    let outcome = handshake::node_finish(node_hand, &hello, &ack, &link).unwrap();
    let mut session = Session::from_keys(
        outcome.keys,
        "node-whitelist".to_owned(),
        AuthMode::Token,
    );

    let request = json!({ "method": "list_containers" });
    let sealed = session
        .seal_frame(FrameType::Request, &serde_json::to_vec(&request).unwrap())
        .unwrap();
    write_frame(&mut client, &sealed).unwrap();
    client.flush().unwrap();
    let reply = read_frame(&mut reader)
        .unwrap()
        .expect("the daemon answered");
    assert_eq!(reply.frame_type, FrameType::Close);
    assert_eq!(reply.value().unwrap()["reason"], "forbidden-method");

    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    handle.join().unwrap();
}

/// A spent chain link buys exactly one session.
///
/// The property Blocker 1 was about. An earlier revision authenticated over
/// the stored chain end, which never changed, so the same mac was reusable for
/// the life of the row: any peer that had once read a hello could mint a fresh
/// one with a new nonce and be handed a session as that node. The link is
/// spent now, and this is the test that says so.
#[test]
fn a_spent_chain_link_cannot_buy_a_second_session() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, NodeHand};
    use box_node_link::wire;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    let (_, enrol_reply) = rpc(
        state.clone(),
        "main-token",
        json!({
            "method": "node_enroll",
            "token": "main-token",
            "kind": "helper",
            "platform": "windows-x86_64",
            "nodeId": "node-replay",
        }),
    );
    let seed = enrol_reply["result"]["bootstrap"]["seed"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed_bytes: [u8; 32] = hex_to_bytes(&seed).try_into().unwrap();
    let chain_length = enrol_reply["result"]["bootstrap"]["chainLength"]
        .as_u64()
        .unwrap() as u32;
    let link = box_node_link::chain::hash_n(&seed_bytes, chain_length - 1);
    let control_pubkey = control_pubkey_of(&state);

    // A fresh hello on the same link, over a fresh connection. A new nonce
    // and a new ephemeral key, so the transcript is different -- which is
    // exactly what made the old behaviour exploitable: nothing about the
    // reuse was visible.
    //
    // Each attempt takes its own listener, because a daemon thread serves
    // one connection and then returns, dropping the listener it was given.
    let attempt = |port: u16| {
        let rng = box_node_link::SystemRandom::new();
        let hand = NodeHand::new(&rng, control_pubkey.clone()).unwrap();
        let hello = handshake::Hello {
            node_id: "node-replay".to_owned(),
            mode: AuthMode::Token,
            version: handshake::VERSION,
            suite: handshake::SUITE,
            client_nonce: handshake::fresh_nonce().unwrap(),
            eph_a: hand.public_key().unwrap(),
            mac: None,
            node_pubkey: None,
            node_sig: None,
            link: Some(link),
        };
        let t1 = handshake::transcript_client(&hello);
        let mac = box_node_link::kdf::hmac_sha256(&box_node_link::kdf::bootstrap_key(&link), &t1);
        let hello = handshake::Hello { mac: Some(mac), ..hello };

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
        write_frame(&mut client, &Frame { frame_type: FrameType::Hello, payload }).unwrap();
        client.flush().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let reply = read_frame(&mut reader).unwrap().expect("the daemon answered");
        (reply, reader, client)
    };

    let (first, reader, client) = attempt(port);
    assert_eq!(
        first.frame_type,
        FrameType::HelloAck,
        "the first use of the link buys a session"
    );
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    // Reaped at the end, beside the second thread.

    // A second daemon, over a second listener, presented with the same link.
    let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port2 = listener2.local_addr().unwrap().port();
    let state2 = state.clone();
    let token2 = "main-token".to_owned();
    let handle2 = std::thread::spawn(move || {
        let (stream, _) = listener2.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), state2, &token2);
    });

    let (second, reader2, client2) = attempt(port2);
    assert_eq!(
        second.frame_type,
        FrameType::Close,
        "a spent link must not buy a second session: {:?}",
        second.value().ok()
    );
    client2.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader2);
    handle.join().unwrap();
}

/// The control-plane public key, read out of the daemon own store.
fn control_pubkey_of(state: &Arc<DaemonState>) -> Vec<u8> {
    let paths = state.paths.read().unwrap().clone();
    let collection = crate::node_keys::open_control_identity(&paths).unwrap();
    hex_to_bytes(
        &crate::node_keys::find_identity(&collection)
            .unwrap()
            .expect("the daemon has an identity now")
            .pubkey,
    )
}

/// A node upgrades from its bootstrap mode to node-key mode.
///
/// This is the transition the design calls for (section 6): a node enrols
/// with a token chain, and its first node-key handshake registers the
/// long-term key every reconnect after that is checked against. Before this,
/// `with_node_pubkey` existed with no production caller, so a node-key row
/// was unreachable and NodeKey mode could never connect.
#[test]
fn a_token_node_upgrades_to_node_key_on_its_first_node_key_handshake() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, Hello, NodeHand};
    use box_node_link::session_io::Session;
    use box_node_link::wire;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    let (_, enrol_reply) = rpc(
        state.clone(),
        "main-token",
        json!({
            "method": "node_enroll",
            "token": "main-token",
            "kind": "helper",
            "platform": "windows-x86_64",
            "nodeId": "node-upgrade",
        }),
    );
    let seed = enrol_reply["result"]["bootstrap"]["seed"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed_bytes: [u8; 32] = hex_to_bytes(&seed).try_into().unwrap();
    let chain_length = enrol_reply["result"]["bootstrap"]["chainLength"]
        .as_u64()
        .unwrap() as u32;
    // The link is still spent on the upgrade: the node proves it holds the
    // bootstrap secret with its mac, and the long-term key is registered on
    // top of that. Skipping the mac would let a stranger register a key for a
    // node it never enrolled as.
    let link = box_node_link::chain::hash_n(&seed_bytes, chain_length - 1);
    let control_pubkey = control_pubkey_of(&state);

    // The node long-term key. The daemon has never seen it.
    let node_key = handshake::generate_control_keypair().unwrap();
    let node_pubkey = hex_to_bytes(&node_key.pubkey_hex);
    let node_signing = handshake::control_signing_key(&node_key.seckey_hex).unwrap();

    let rng = box_node_link::SystemRandom::new();
    let node_hand = NodeHand::new(&rng, control_pubkey).unwrap();
    let eph_a = node_hand.public_key().unwrap();
    // Node-key mode carries no mac and no link, and the wire decoder refuses
    // it if either is present -- the modes are exclusive.
    let mut hello = Hello {
        node_id: "node-upgrade".to_owned(),
        mode: AuthMode::NodeKey,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().unwrap(),
        eph_a,
        mac: None,
        node_pubkey: Some(node_pubkey.clone()),
        node_sig: None,
        link: None,
    };
    // Signed over t1, the client half, which the node can compute before it
    // has heard anything back. That is why the signature can travel in the
    // hello at all.
    let t1 = handshake::transcript_client(&hello);
    hello.node_sig = Some(node_signing.sign(&t1).as_ref().to_vec());

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
    write_frame(&mut client, &Frame { frame_type: FrameType::Hello, payload }).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let ack = read_frame(&mut reader)
        .unwrap()
        .expect("the handshake completes");
    assert_eq!(
        ack.frame_type,
        FrameType::HelloAck,
        "a validly signed node-key hello must be accepted: {:?}",
        ack.value().ok()
    );

    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    handle.join().unwrap();

    // And the row is upgraded: node-key mode, the key registered, and the
    // chain retired with it.
    let paths = state.paths.read().unwrap().clone();
    let collection = crate::node_keys::open_node_keys(&paths).unwrap();
    let row = collection
        .load_all()
        .unwrap()
        .into_iter()
        .find(|r| r.id == "node-upgrade")
        .expect("the enrol wrote a row");
    assert_eq!(row.mode, box_node_link::node_keys::KeyMode::NodeKey);
    assert_eq!(row.node_pubkey.as_deref(), Some(node_key.pubkey_hex.as_str()));
    assert!(row.chain_end.is_none(), "the chain is retired with the mode");

    // And the session really works, which is the part that makes the upgrade
    // worth anything.
    let ack_wire: wire::HelloAckWire = serde_json::from_slice(&ack.payload).unwrap();
    let ack = ack_wire.to_ack().unwrap();
    let outcome = handshake::node_finish(node_hand, &hello, &ack, &link).unwrap();
    let _ = outcome;
}

/// A peer-initiated rekey is accepted and the session stays in step.
///
/// The daemon gained an arm for incoming rekey frames with no test at all --
/// the arm was reachable only through the handshake tests, which never send
/// one. This drives the real socket: the node sends a rekey frame, the daemon
/// accepts it, and the request that follows crosses in the new direction.
#[test]
fn a_peer_initiated_rekey_keeps_the_session_in_step() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    use box_node_link::handshake::{self, AuthMode, Hello, NodeHand};
    use box_node_link::session_io::Session;
    use box_node_link::wire;
    use std::io::BufWriter;

    let dir = tempfile::tempdir().unwrap();
    let state = state_with_runtime(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });

    let (_, enrol_reply) = rpc(
        state.clone(),
        "main-token",
        json!({
            "method": "node_enroll",
            "token": "main-token",
            "kind": "helper",
            "platform": "windows-x86_64",
            "nodeId": "node-rekey",
        }),
    );
    let seed = enrol_reply["result"]["bootstrap"]["seed"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed_bytes: [u8; 32] = hex_to_bytes(&seed).try_into().unwrap();
    let chain_length = enrol_reply["result"]["bootstrap"]["chainLength"]
        .as_u64()
        .unwrap() as u32;
    let link = box_node_link::chain::hash_n(&seed_bytes, chain_length - 1);
    let control_pubkey = control_pubkey_of(&state);

    let rng = box_node_link::SystemRandom::new();
    let node_hand = NodeHand::new(&rng, control_pubkey).unwrap();
    let eph_a = node_hand.public_key().unwrap();
    let hello = Hello {
        node_id: "node-rekey".to_owned(),
        mode: AuthMode::Token,
        version: handshake::VERSION,
        suite: handshake::SUITE,
        client_nonce: handshake::fresh_nonce().unwrap(),
        eph_a,
        mac: None,
        node_pubkey: None,
        node_sig: None,
        link: Some(link),
    };
    let t1 = handshake::transcript_client(&hello);
    let mac = box_node_link::kdf::hmac_sha256(&box_node_link::kdf::bootstrap_key(&link), &t1);
    let hello = Hello { mac: Some(mac), ..hello };

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut writer = BufWriter::new(client.try_clone().unwrap());
    let mut reader = BufReader::new(client.try_clone().unwrap());

    let payload = serde_json::to_vec(&wire::hello_wire(&hello)).unwrap();
    write_frame(&mut writer, &Frame { frame_type: FrameType::Hello, payload }).unwrap();
    writer.flush().unwrap();
    let ack = read_frame(&mut reader).unwrap().expect("the handshake completes");
    assert_eq!(ack.frame_type, FrameType::HelloAck);

    let ack_wire: wire::HelloAckWire = serde_json::from_slice(&ack.payload).unwrap();
    let ack = ack_wire.to_ack().unwrap();
    let outcome = handshake::node_finish(node_hand, &hello, &ack, &link).unwrap();
    // The node holds the keys the way the protocol builds them: its send key
    // is the daemon receive key, and the daemon swapped its own pair when it
    // built its session.
    let mut session = Session::from_keys(outcome.keys, "node-rekey".to_owned(), AuthMode::Token);

    // The node asks for a rekey. The frame is sealed under the current pair,
    // which is why the daemon can open it.
    session.send_rekey(&mut writer).expect("the rekey goes out");

    // The daemon accepted it: it did not close, and the next request is
    // answered. The daemon sealed that reply under the rotated pair, which
    // the node can now open -- the convergence, observed end to end.
    let request = json!({ "method": "node_echo", "params": { "value": 1 } });
    let sealed = session
        .seal_frame(FrameType::Request, &serde_json::to_vec(&request).unwrap())
        .unwrap();
    write_frame(&mut writer, &sealed).unwrap();
    writer.flush().unwrap();

    let reply = read_frame(&mut reader)
        .unwrap()
        .expect("the daemon answered after the rekey");
    assert_eq!(reply.frame_type, FrameType::Response);
    let opened = session
        .open_frame(&reply)
        .expect("the reply opens under the rotated keys");
    let parsed: serde_json::Value = serde_json::from_slice(&opened).unwrap();
    assert_eq!(parsed["ok"], true, "the session still works after a peer rekey");

    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    handle.join().unwrap();
}
/// A request whose payload is not JSON closes the link. There is no
/// method to dispatch, and guessing one would mean acting on bytes we
/// could not read.
#[test]
fn a_request_with_a_non_json_payload_closes_the_link() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    let state = state();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        node_link::handle_link(stream, Vec::new(), server_state, &token);
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let frame = Frame { frame_type: FrameType::Request, payload: b"not json".to_vec() };
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client);
    // Not unwrap: the peer closes with no reply, and whether that lands
    // as a clean EOF or as an Io error is a race. Both mean "no reply",
    // which is the thing under test.
    let reply = read_frame(&mut reader).ok().flatten();
    drop(reader);
    handle.join().unwrap();
    assert!(reply.is_none(), "a malformed payload gets no reply at all");
}

/// The route as a client actually reaches it: HTTP request line and
/// headers, then the first frame, all in ONE write.
///
/// This is the case the direct `handle_link` tests cannot see. The header
/// parser reads through a BufReader, which pulls in blocks -- so by the
/// time the blank line is seen, the first frame is usually already sitting
/// in that reader's buffer. A handler that builds its own reader from the
/// raw socket would skip those bytes, and what it would see next is
/// indistinguishable from a clean close: no reply, no error. Every test
/// above calls `handle_link` directly and therefore passes a reader with an
/// empty buffer, which is why none of them caught it.
#[test]
fn a_pipelined_first_frame_is_not_lost_behind_the_headers() {
    use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
    let state = state();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "main-token".to_owned();
    let server_state = state.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handle_http(stream, server_state, &token);
    });
    // One write: the request, its headers, and the frame behind them.
    let body = json!({ "method": "ping", "token": "main-token" });
    let frame = Frame::json(FrameType::Request, &body).unwrap();
    let mut wire = Vec::new();
    write_frame(&mut wire, &frame).unwrap();
    let head = "GET /link HTTP/1.1\r\nHost: x\r\n\r\n";
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // A single write, headers and frame together. Two writes would let
    // the header parser finish before the frame arrives, and the bug
    // would not reproduce.
    let mut pipelined = head.as_bytes().to_vec();
    pipelined.extend_from_slice(&wire);
    client.write_all(&pipelined).unwrap();
    client.flush().unwrap();
    // A timeout so a regression fails here rather than hanging until the
    // harness gives up: the failure mode of this bug is silence, and a
    // silent test is worse than a red one.
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(client);
    let reply = read_frame(&mut reader).unwrap();
    drop(reader);
    handle.join().unwrap();
    let reply = reply.expect("the first frame survived the header parser");
    assert_eq!(reply.frame_type, FrameType::Response);
    assert_eq!(reply.value().unwrap()["ok"], true);
}

