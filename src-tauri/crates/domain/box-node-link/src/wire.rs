//! The wire types for the node link handshake.
//!
//! These are what actually travels: hello and hello-ack as JSON payloads
//! inside frames. The handshake module (handshake.rs) works on the parsed
//! structures; this module is the boundary between the two, and it is where
//! the "unknown field is refused" discipline lives.
//!
//! Every field is camelCase on the wire, matching the rest of the daemon's
//! RPC surface. A field that is not understood is an error rather than a
//! silently dropped one, because a dropped field is how a downgrade looks
//! like a success.

use serde::{Deserialize, Serialize};

use crate::handshake::{AuthMode, Hello, HelloAck, SUITE, VERSION};

/// The node's first frame, as it travels.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloWire {
    pub version: u16,
    pub suite: u16,
    pub node_id: String,
    pub mode: AuthMode,
    /// 16 random bytes, hex.
    pub client_nonce: String,
    /// The node's ephemeral X25519 public key, hex.
    pub eph_a: String,
    /// psk/token: HMAC over t1, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// node-key: the node's long-term public key, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_pubkey: Option<String>,
    /// node-key: an Ed25519 signature over t1, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_sig: Option<String>,
    /// token: the chain link the node is spending, c_k, hex.
    ///
    /// Without it the control plane cannot spend the link -- it holds the
    /// chain end and accepts a value only when H(value) equals it, so a
    /// value it never receives is a link it never spends. An earlier
    /// revision authenticated over the chain end instead, which meant the
    /// same mac was reusable for the life of the row: any peer that had
    /// once read a hello could mint a fresh one with a new nonce and be
    /// handed a session as that node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// The control plane's reply, as it travels.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloAckWire {
    /// 16 random bytes, hex.
    pub server_nonce: String,
    /// The control plane's ephemeral X25519 public key, hex.
    pub eph_b: String,
    /// Ed25519 over t2, hex. Required in every mode.
    pub control_sig: String,
    /// The sealed session certificate.
    pub sealed_cert: String,
}

/// Why a frame could not be turned into a handshake message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("a hex field is not valid hex: {field}")]
    BadHex { field: &'static str },
    #[error("a field has the wrong length: {field}")]
    BadLength { field: &'static str },
    #[error("the mode requires {field}, which is missing")]
    MissingField { field: &'static str },
    #[error("this peer offers version {offered}, but this build speaks {supported}")]
    UnsupportedVersion { offered: u16, supported: u16 },
    #[error("this peer offers suite {offered}, but this build speaks {supported}")]
    UnsupportedSuite { offered: u16, supported: u16 },
    #[error("mode {mode:?} carries a field that belongs to another mode")]
    WrongModeField { mode: AuthMode },
}

impl HelloWire {
    /// Turn the wire form into the handshake form, refusing anything that
    /// does not fit the mode.
    ///
    /// The mode check is the point: a psk hello carrying a node_sig, or a
    /// node-key hello carrying a mac, is a peer trying to be two things at
    /// once. Refusing it here means the handshake never has to consider a
    /// half-populated hello.
    pub fn to_hello(&self) -> Result<Hello, WireError> {
        if self.version != VERSION {
            return Err(WireError::UnsupportedVersion {
                offered: self.version,
                supported: VERSION,
            });
        }
        if self.suite != SUITE {
            return Err(WireError::UnsupportedSuite {
                offered: self.suite,
                supported: SUITE,
            });
        }
        let mac = match &self.mac {
            Some(hex) => Some(decode32(hex, "mac")?),
            None => None,
        };
        let node_pubkey = match &self.node_pubkey {
            Some(hex) => Some(decode_vec(hex, "nodePubkey")?),
            None => None,
        };
        let node_sig = match &self.node_sig {
            Some(hex) => Some(decode_vec(hex, "nodeSig")?),
            None => None,
        };
        let link = match &self.link {
            Some(hex) => Some(decode32(hex, "link")?),
            None => None,
        };
        match self.mode {
            AuthMode::Psk => {
                if mac.is_none() {
                    return Err(WireError::MissingField { field: "mac" });
                }
                if node_pubkey.is_some() || node_sig.is_some() || link.is_some() {
                    return Err(WireError::WrongModeField { mode: self.mode });
                }
            }
            AuthMode::Token => {
                if mac.is_none() {
                    return Err(WireError::MissingField { field: "mac" });
                }
                // Without the link the control plane cannot spend it, and an
                // unspent link is a reusable credential.
                if link.is_none() {
                    return Err(WireError::MissingField { field: "link" });
                }
                if node_pubkey.is_some() || node_sig.is_some() {
                    return Err(WireError::WrongModeField { mode: self.mode });
                }
            }
            AuthMode::NodeKey => {
                if node_pubkey.is_none() || node_sig.is_none() {
                    return Err(WireError::MissingField {
                        field: "nodePubkey/nodeSig",
                    });
                }
                if mac.is_some() {
                    return Err(WireError::WrongModeField { mode: self.mode });
                }
            }
        }
        Ok(Hello {
            node_id: self.node_id.clone(),
            mode: self.mode,
            version: self.version,
            suite: self.suite,
            client_nonce: decode16(&self.client_nonce, "clientNonce")?,
            eph_a: decode32(&self.eph_a, "ephA")?,
            mac,
            node_pubkey,
            node_sig,
            link,
        })
    }
}

impl HelloAckWire {
    pub fn to_ack(&self) -> Result<HelloAck, WireError> {
        Ok(HelloAck {
            server_nonce: decode16(&self.server_nonce, "serverNonce")?,
            eph_b: decode32(&self.eph_b, "ephB")?,
            control_sig: decode_vec(&self.control_sig, "controlSig")?,
            sealed_cert: decode_vec(&self.sealed_cert, "sealedCert")?,
        })
    }
}

/// Build the wire form of a hello.
pub fn hello_wire(hello: &Hello) -> HelloWire {
    HelloWire {
        version: hello.version,
        suite: hello.suite,
        node_id: hello.node_id.clone(),
        mode: hello.mode,
        client_nonce: encode(&hello.client_nonce),
        eph_a: encode(&hello.eph_a),
        mac: hello.mac.map(|m| encode(&m)),
        node_pubkey: hello.node_pubkey.as_deref().map(encode),
        node_sig: hello.node_sig.as_deref().map(encode),
        link: hello.link.map(|l| encode(&l)),
    }
}

/// Build the wire form of an ack.
pub fn ack_wire(ack: &HelloAck) -> HelloAckWire {
    HelloAckWire {
        server_nonce: encode(&ack.server_nonce),
        eph_b: encode(&ack.eph_b),
        control_sig: encode(&ack.control_sig),
        sealed_cert: encode(&ack.sealed_cert),
    }
}

fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode hex, naming the field in the error so a caller can tell which
/// one was malformed.
fn decode_vec(text: &str, field: &'static str) -> Result<Vec<u8>, WireError> {
    if text.len() % 2 != 0 {
        return Err(WireError::BadHex { field });
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(hi), Some(lo)) => out.push((hi * 16 + lo) as u8),
            _ => return Err(WireError::BadHex { field }),
        }
    }
    Ok(out)
}

fn decode32(text: &str, field: &'static str) -> Result<[u8; 32], WireError> {
    let bytes = decode_vec(text, field)?;
    bytes
        .try_into()
        .map_err(|_| WireError::BadLength { field })
}

fn decode16(text: &str, field: &'static str) -> Result<[u8; 16], WireError> {
    let bytes = decode_vec(text, field)?;
    bytes
        .try_into()
        .map_err(|_| WireError::BadLength { field })
}
