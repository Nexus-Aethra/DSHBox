//! The node-keys store: the control plane index of node bootstrap secrets.
//!
//! Stage L4 of the connection track. It owns the storage half of design doc
//! section 8 -- one row per node in the document store -- and none of the
//! handshake, which lives in box-node-link.
//!
//! The four disciplines from that section are why this module is shaped the
//! way it is:
//!
//! 1. The secret never reaches a log line. There is no tracing call in this
//!    file at all, and the record Debug is hand-written in box-node-link to
//!    redact it.
//! 2. The database is 0600. It is its own file (secrets.db), not the main
//!    create, so a store left loose by an earlier build is repaired.
//! 3. The token seed is never stored. A token row holds the chain end and the
//!    last accepted link, so a stolen row cannot regenerate the chain.
//! 4. Issuing reveals the secret once. issue returns the plaintext for that
//!    one delivery and stores only what verification needs.

use box_foundation::collection::{Collection, DocumentStore};
use box_foundation::{now_seconds, BoxPaths, BoxResult};
use box_node_link::node_keys::{KeyMode, NodeKeyRecord, NodeKeySummary};
use box_store::SqliteDocumentStore;
use serde::{Deserialize, Serialize};

/// New scope, same store. Adding a scope is a new row, not a migration: the
/// schema version stays at v1 (design doc section 5, and the same reasoning
/// node-artifacts records).
pub const NODE_KEY_SCOPE: &str = "node-keys";

/// The control plane own long-term identity, one row in its own scope.
///
/// Separate from the node rows because who-am-I and who-may-speak-to-me are
/// two different facts, and a single row holding both would make rotating
/// one silently rotate the other.
pub const CONTROL_IDENTITY_SCOPE: &str = "control-identity";

/// The document key of a node-key row.
pub fn node_key(record: &NodeKeyRecord) -> &str {
    record.id.as_str()
}

/// The document key of the control identity row.
pub fn identity_key(record: &ControlIdentity) -> &str {
    record.id.as_str()
}

/// The single control-plane identity row.
///
/// The private key is stored as PKCS#8 DER, hex. It is a secret and follows
/// the same disciplines as a node secret: never logged, and only the
/// fingerprint leaves this module.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlIdentity {
    /// Always "default". One store, one control plane.
    pub id: String,
    /// Ed25519 public key, hex.
    pub pubkey: String,
    /// Ed25519 private key as PKCS#8 DER, hex.
    pub seckey: String,
    /// Short fingerprint of the public key: what a node pins offline.
    pub fingerprint: String,
    pub created_at: u64,
}

impl std::fmt::Debug for ControlIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same rule as NodeKeyRecord: the private half is redacted, the public
        // half and the fingerprint are shown.
        f.debug_struct("ControlIdentity")
            .field("id", &self.id)
            .field("pubkey", &self.pubkey)
            .field("seckey", &"<redacted>")
            .field("fingerprint", &self.fingerprint)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// The node-keys collection, opened against a runtime directory.
///
/// Falls back to an in-memory collection when no runtime is configured, the
/// same way node-artifacts does: a host with no runtime directory has nowhere
/// to put rows, and the alternative is failing a read that has nothing to
/// read.
/// Open the secrets store, falling back to memory when no runtime is set.
///
/// The fallback is the same one node-artifacts uses: a host with no runtime
/// directory has nowhere to put rows, and the alternative is failing a read
/// that has nothing to read.
fn secrets_store(paths: &BoxPaths) -> BoxResult<std::sync::Arc<dyn DocumentStore>> {
    let path = paths.secrets_db()?;
    Ok(std::sync::Arc::new(SqliteDocumentStore::open_secrets(&path)?))
}

pub fn open_node_keys(paths: &BoxPaths) -> BoxResult<Collection<NodeKeyRecord>> {
    match secrets_store(paths) {
        Ok(store) => Ok(Collection::new(store, NODE_KEY_SCOPE, node_key)),
        Err(_) => Ok(Collection::memory(NODE_KEY_SCOPE, node_key)),
    }
}

/// The control-identity collection.
pub fn open_control_identity(paths: &BoxPaths) -> BoxResult<Collection<ControlIdentity>> {
    match secrets_store(paths) {
        Ok(store) => Ok(Collection::new(store, CONTROL_IDENTITY_SCOPE, identity_key)),
        Err(_) => Ok(Collection::memory(CONTROL_IDENTITY_SCOPE, identity_key)),
    }
}

/// What a caller may learn about a node key, without the key.
///
/// The record itself is deliberately not returned anywhere: summary is the
/// only shape that leaves this module for an RPC.
pub fn summary_of(record: &NodeKeyRecord) -> NodeKeySummary {
    record.summary()
}

/// The registered node key for a node, if it has one.
///
/// This is what a node-key handshake is checked against: the row written at
/// first enrolment, and nothing else. A node that has never enrolled has no
/// registered key and must use psk or token.
pub fn registered_pubkey_for(
    collection: &Collection<NodeKeyRecord>,
    node_id: &str,
) -> BoxResult<Option<String>> {
    Ok(collection
        .load_all()?
        .into_iter()
        .find(|record| record.id == node_id)
        .and_then(|record| record.node_pubkey))
}

/// Is this node allowed to start a handshake at all?
///
/// Revoked and expired are both refusals, checked here so the two cannot
/// drift apart between the enrol path and the reconnect path.
pub fn is_node_live(record: &NodeKeyRecord) -> bool {
    record.is_live(now_seconds())
}

/// The mode a node enrolled with, for the incompatible-mode refusal.
pub fn mode_of(record: &NodeKeyRecord) -> KeyMode {
    record.mode
}

/// Is this node id usable as a store key?
///
/// The document id is one TEXT column, and a node id travels through URLs and
/// JSON, so it is restricted to a conservative set rather than to anything
/// without a separator. A node id with a path separator or a control
/// character is refused at the boundary instead of being escaped later.
pub fn is_node_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The control plane signing key, generated once.
///
/// Returns the fingerprint either way, because that is what a node pins
/// offline and what an operator reads off the machine. The key is generated
/// through box-node-link so the primitive and the encoding stay in one place;
/// the private half never leaves this module except inside the stored row.
pub fn ensure_control_identity(
    collection: &Collection<ControlIdentity>,
) -> BoxResult<String> {
    if let Some(existing) = find_identity(collection)? {
        return Ok(existing.fingerprint);
    }
    let generated = box_node_link::handshake::generate_control_keypair()
        .map_err(|error| error.to_string())?;
    let (pubkey, seckey, fingerprint) = (
        generated.pubkey_hex,
        generated.seckey_hex,
        generated.fingerprint,
    );
    let identity = ControlIdentity {
        id: "default".to_owned(),
        pubkey,
        seckey,
        fingerprint: fingerprint.clone(),
        created_at: now_seconds(),
    };
    collection.upsert(&[identity])?;
    Ok(fingerprint)
}

/// The stored identity, if there is one.
pub fn find_identity(
    collection: &Collection<ControlIdentity>,
) -> BoxResult<Option<ControlIdentity>> {
    Ok(collection
        .load_all()?
        .into_iter()
        .find(|record| record.id == "default"))
}

#[cfg(test)]
#[path = "node_keys_tests.rs"]
mod tests;
