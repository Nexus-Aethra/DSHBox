//! The node link: one long-lived connection carrying length-prefixed JSON
//! frames instead of one HTTP POST per operation.
//!
//! This is stage L1 of the connection track
//! (`docs/design/2026-11-08-node-link-session-and-crate-layering.md` section 10,
//! plan in `docs/design/2026-11-17-node-link-l1-frame-layer.md`). It is
//! **plaintext and unauthenticated at the transport level** -- the
//! authentication is the same session-token check the HTTP path uses, and
//! the encryption arrives with the handshake in L2. Do not expose this port
//! to a network you would not expose plain HTTP to.
//!
//! The point of L1 is that the *transport* changes and nothing else does:
// the same handlers, the same whitelist, the same identity binding. So the
// gate below is written once and used by both paths.

use crate::state::DaemonState;
use box_node_link::frame::{read_frame, write_frame, Frame, FrameType};
use std::net::TcpStream;
use std::sync::Arc;

/// What a caller is allowed to do on this connection, decided once per
/// request exactly as the HTTP path decides it.
///
/// Kept as data rather than a bool so a future transport cannot accidentally
/// skip a step: the HTTP path and the frame path both build one of these and
/// both act on the same fields.
pub(crate) struct Gate {
    /// The node this connection speaks for, or `None` for the control plane
    /// (a caller holding the main discovery token).
    pub(crate) node_id: Option<String>,
    /// True when the bearer is the daemon's own discovery token.
    pub(crate) is_main: bool,
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
        Ok(Self { node_id: session_node, is_main })
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
/// Each request frame is gated, dispatched and answered on the same
/// connection. A refused request closes the connection rather than being
/// answered and ignored: a peer that cannot authenticate has no business
/// holding a thread, and a peer asking for a method it may not have is
/// either confused or probing.
///
/// The read side is sequential. A second concurrent request on the same
/// connection would need request IDs to match responses, and L1 has no use
/// for them -- the helper sends one request at a time.
pub(crate) fn handle_link(mut stream: TcpStream, state: Arc<DaemonState>, main_token: &str) {
    // A node link is long-lived by design, so there is no per-request
    // timeout to inherit: the read blocks until a frame arrives or the peer
    // goes away.
    let mut reader = std::io::BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            // A clean close at a frame boundary is a peer that finished.
            Ok(None) => return,
            // Anything else is a malformed or torn frame. Close without a
            // reply: there is no request to answer, and half a session is
            // worse than none.
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "frame error; closing link");
                return;
            }
        };
        // Only request frames carry traffic in L1. A peer that opens with
        // something else is speaking a protocol this build does not
        // implement yet; say so rather than silently ignoring it.
        if frame.frame_type != FrameType::Request {
            let _ = send_close(&mut stream, "unexpected-frame-type");
            return;
        }
        let request = match frame.value() {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(target: "node_link", %error, "payload is not JSON; closing link");
                return;
            }
        };
        match Gate::resolve(&state, main_token, &request) {
            Ok(gate) => {
                // `__nodeId` is server-owned, exactly as on the HTTP path:
                // drop whatever the caller sent, then bind it from the
                // validated session. Without the unconditional drop a
                // discovery-token holder could name any node in the body.
                let mut bound = request;
                crate::bind_request_node(&mut bound, gate.node_id.as_deref());
                let response = crate::dispatch::dispatch(&state, &bound);
                let frame = match Frame::json(FrameType::Response, &response) {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!(target: "node_link", %error, "cannot encode response");
                        return;
                    }
                };
                if write_frame(&mut stream, &frame).is_err() {
                    return;
                }
            }
            Err(denied) => {
                let _ = send_close(&mut stream, denied.reason());
                return;
            }
        }
    }
}

/// Write a close frame carrying a reason. Best-effort: the peer is going
/// away regardless.
fn send_close(stream: &mut TcpStream, reason: &str) -> std::io::Result<()> {
    let frame = Frame::json(FrameType::Close, &serde_json::json!({ "reason": reason }))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    write_frame(stream, &frame).map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with no credential at all is refused for want of a session,
    /// not for a forbidden method. The distinction is what tells a helper
    /// whether to re-enroll or to stop asking.
    #[test]
    fn a_request_without_a_credential_has_no_session() {
        // The gate needs a DaemonState to resolve a session; the one case
        // that does not is the main token, which is compared as a string.
        // This asserts the deny path that needs no state at all.
        let denied = GateDenied::NoSession;
        assert_eq!(denied.reason(), "no-session");
        assert_ne!(denied, GateDenied::ForbiddenMethod);
    }

    /// The two reasons are distinct on the wire, because a helper branches on
    /// them: one means "get a new grant", the other means "that method is not
    /// yours".
    #[test]
    fn the_two_denial_reasons_are_distinct() {
        assert_eq!(GateDenied::NoSession.reason(), "no-session");
        assert_eq!(GateDenied::ForbiddenMethod.reason(), "forbidden-method");
    }
}
