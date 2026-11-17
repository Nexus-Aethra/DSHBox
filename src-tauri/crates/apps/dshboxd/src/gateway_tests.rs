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
    state.nodes.register(&grant, None, None).unwrap();

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
    let (_, session, epoch, _) = state.nodes.register(&g1, None, None).unwrap();
    let (g2, b, _, _) = state
        .nodes
        .enroll("wsl2", "linux-x86_64", vec![], Some("node-b".to_owned()))
        .unwrap();
    state.nodes.register(&g2, None, None).unwrap();

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

/// A frame that is not a request closes the link. L1 has no handshake,
/// so a peer that opens with `hello` is speaking a protocol this build
/// does not implement; answering it would imply that it does.
#[test]
fn a_non_request_frame_closes_the_link() {
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
    // `hello` is the L2 handshake frame. It is declared in the codec but
    // nothing in L1 emits or answers it.
    let frame = Frame::json(FrameType::Hello, &serde_json::json!({ "ver": 1 })).unwrap();
    write_frame(&mut client, &frame).unwrap();
    client.flush().unwrap();
    let mut reader = BufReader::new(client);
    let reply = read_frame(&mut reader).unwrap();
    drop(reader);
    handle.join().unwrap();
    let reply = reply.expect("the link answered before closing");
    assert_eq!(reply.frame_type, FrameType::Close, "a hello is not answered");
    assert_eq!(reply.value().unwrap()["reason"], "unexpected-frame-type");
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

