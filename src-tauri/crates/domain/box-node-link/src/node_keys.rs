//! The node-keys record: what the control plane stores about a node bootstrap
//! secret.
//!
//! This is the record, not the store: a plain struct plus the rules for
//! reading it, testable without a database, with the daemon or the desktop
//! supplying persistence. The design doc section 8 gives four disciplines,
//! and each is enforced here or nowhere.
//!
//! 1. The secret never reaches a log line. Debug is hand-written and redacts
//!    it, and the RPC surface is a separate summary type that has no secret
//!    field to forget to strip.
//! 2. The library file is 0600. That is the caller job -- this module never
//!    opens a file itself.
//! 3. Three things never enter the store: session keys (memory only), the
//!    node private key (node side, 0600), and the token seed. The control
//!    plane keeps the chain end and the last accepted link, which are not
//!    the secret.
//! 4. Issuing reveals the secret exactly once. `issue` hands the plaintext
//!    back for that one delivery; the record keeps only what verification
//!    needs.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::handshake::AuthMode;

/// How a node authenticates, as stored.
///
/// The mode is part of the record rather than inferred from which fields are
/// populated, because an empty field is ambiguous between "not applicable"
/// and "not yet issued".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyMode {
    /// A pre-shared key, delivered once at enrolment.
    Psk,
    /// A hash chain. The control plane stores the chain end and the last
    /// accepted link, never the seed.
    Token,
    /// A registered node key. The control plane stores the public key.
    NodeKey,
}

impl KeyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyMode::Psk => "psk",
            KeyMode::Token => "token",
            KeyMode::NodeKey => "node-key",
        }
    }
}

impl From<AuthMode> for KeyMode {
    fn from(mode: AuthMode) -> Self {
        match mode {
            AuthMode::Psk => KeyMode::Psk,
            AuthMode::Token => KeyMode::Token,
            AuthMode::NodeKey => KeyMode::NodeKey,
        }
    }
}

/// One node key record.
///
/// One row per node, keyed by node id. Re-issuing overwrites the row rather
/// than appending, so "which key is current" does not need a timestamp to
/// answer.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeKeyRecord {
    /// The node this row belongs to. Also the primary key.
    pub id: String,
    pub mode: KeyMode,
    /// psk: the pre-shared key. token: empty, because the seed is never
    /// stored here and a stolen row must not regenerate the chain. node-key:
    /// empty.
    pub secret: String,
    /// The value the next handshake must be the preimage of. None until the
    /// node has connected once, in which case the chain end is expected.
    pub last_accepted: Option<String>,
    /// H^n(seed): the end of the hash chain, the only chain value the
    /// control plane holds.
    pub chain_end: Option<String>,
    /// The node Ed25519 public key, hex. Written on first registration; it is
    /// the node identity for every reconnect after that.
    pub node_pubkey: Option<String>,
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
    /// The helper build that enrolled, for the incompatible-helper refusal.
    /// Not a security field.
    pub helper_version: Option<String>,
}

impl fmt::Debug for NodeKeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The secret is the one field that must never appear in a log line,
        // including a debug one. Everything else is shown.
        f.debug_struct("NodeKeyRecord")
            .field("id", &self.id)
            .field("mode", &self.mode.as_str())
            .field("secret", &"<redacted>")
            .field("last_accepted", &self.last_accepted)
            .field("chain_end", &self.chain_end.as_deref().map(|v| short_digest("node-chain-end-v1", v)))
            .field("node_pubkey", &self.node_pubkey)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("revoked_at", &self.revoked_at)
            .field("helper_version", &self.helper_version)
            .finish()
    }
}

/// What the RPC surface is allowed to see.
///
/// A separate type rather than an Option-wrapped record, because the absence
/// of the secret is the point: there is no field to forget to strip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeKeySummary {
    pub id: String,
    pub mode: KeyMode,
    pub has_key: bool,
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub revoked: bool,
    /// A fingerprint of the node key, for matching against what a node
    /// reports. Not the key.
    pub fingerprint: Option<String>,
}

impl NodeKeyRecord {
    /// A fresh psk record.
    pub fn psk(id: &str, secret: &str, issued_at: u64) -> Self {
        Self {
            id: id.to_owned(),
            mode: KeyMode::Psk,
            secret: secret.to_owned(),
            last_accepted: None,
            chain_end: None,
            node_pubkey: None,
            issued_at,
            expires_at: None,
            revoked_at: None,
            helper_version: None,
        }
    }

    /// A fresh token record, holding only the chain end.
    ///
    /// The seed goes to the node and is not stored: the control plane keeps
    /// H^n(seed) and, after the first connection, the last accepted link.
    pub fn token(id: &str, chain_end: &str, issued_at: u64) -> Self {
        Self {
            id: id.to_owned(),
            mode: KeyMode::Token,
            secret: String::new(),
            last_accepted: None,
            chain_end: Some(chain_end.to_owned()),
            node_pubkey: None,
            issued_at,
            expires_at: None,
            revoked_at: None,
            helper_version: None,
        }
    }

    /// Register the node long-term key. Called on first enrolment; the value
    /// is what every later reconnect is checked against.
    pub fn with_node_pubkey(mut self, pubkey: &str) -> Self {
        self.node_pubkey = Some(pubkey.to_owned());
        self
    }

    /// The key the control plane compares a node-key handshake against.
    pub fn registered_pubkey(&self) -> Option<&str> {
        self.node_pubkey.as_deref()
    }

    /// Is this row usable right now?
    ///
    /// Revoked and expired are both refusals, checked here rather than at
    /// each call site so the two cannot drift apart.
    pub fn is_live(&self, now: u64) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        match self.expires_at {
            Some(expires) => now < expires,
            None => true,
        }
    }

    /// What an RPC caller may see. Never carries the secret.
    pub fn summary(&self) -> NodeKeySummary {
        NodeKeySummary {
            id: self.id.clone(),
            mode: self.mode,
            has_key: match self.mode {
                KeyMode::Psk => !self.secret.is_empty(),
                KeyMode::Token => self.chain_end.is_some(),
                KeyMode::NodeKey => self.node_pubkey.is_some(),
            },
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            revoked: self.revoked_at.is_some(),
            fingerprint: self.node_pubkey.as_deref().map(fingerprint_of),
        }
    }
}

/// A short, stable fingerprint of a node public key.
///
/// Lets an operator match a key read off one machine against the one the
/// control plane holds, without either side printing the key.
pub fn fingerprint_of(pubkey_hex: &str) -> String {
    short_digest("node-key-fingerprint-v1", pubkey_hex)
}

/// A short, stable digest of any chain value.
///
/// Separate from fingerprint_of, and with its own domain prefix, so a chain
/// digest and a key fingerprint can never be mistaken for each other in a
/// log line.
pub fn short_digest(domain: &str, value: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("{domain}:{value}").as_bytes(),
    );
    let hex = digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    // 16 hex chars: enough to tell two keys apart by eye, short enough to
    // read aloud.
    hex[..16].to_owned()
}
