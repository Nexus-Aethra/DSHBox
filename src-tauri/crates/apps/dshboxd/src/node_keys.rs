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

/// A bootstrap secret, minted and ready to hand to a node exactly once.
///
/// Deliberately not `Debug`, and the `seed` field is the reason: a derived
/// `Debug` would make `{:?}` on this struct a way to leak the one value that
/// must never reach a log line. The summary the RPC returns is a different
/// type without the field at all.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedBootstrap {
    pub node_id: String,
    /// The seed, hex. Hand it to the node once, over a channel the operator
    /// controls, and never log it.
    pub seed: String,
    /// How many links the chain has, so the node knows when it runs out.
    pub chain_length: u32,
    /// H^n(seed): the value the first link must be the preimage of. This is
    /// what the daemon stores.
    pub chain_end: String,
    pub issued_at: u64,
}

/// How many links a fresh chain has.
///
/// 64 is the design default: long enough that a node reconnects for months
/// before it must be re-enrolled, short enough that the node can compute the
/// whole chain in memory if it wants to.
pub const BOOTSTRAP_CHAIN_LENGTH: u32 = 64;

/// Mint a bootstrap secret for a node and store only what verification needs.
///
/// The seed goes to the node and nowhere else. The daemon keeps the chain end,
/// which is the value the first link must be the preimage of -- so a stolen
/// row cannot regenerate the chain, which is the property design section 3 is
/// built on.
///
/// The default mode is a token chain rather than a PSK. A PSK is one shared
/// value, so every handshake replays the same bytes on the wire; a chain
/// presents a fresh link each time, and a spent link is refused.
///
/// A node that already has a row is refused rather than re-issued. Re-issuing
/// silently would invalidate the secret the running helper is holding, and a
/// helper that suddenly cannot authenticate is a support call nobody can
/// diagnose from this side.
pub fn issue_bootstrap(
    collection: &Collection<NodeKeyRecord>,
    node_id: &str,
    helper_version: Option<&str>,
) -> BoxResult<IssuedBootstrap> {
    if !is_node_id(node_id) {
        return Err(format!("invalid node id {node_id:?}"));
    }
    if collection
        .load_all()?
        .iter()
        .any(|row| row.id == node_id)
    {
        return Err(format!(
            "node {node_id} already has a bootstrap key; revoke it first"
        ));
    }

    let chain_length = BOOTSTRAP_CHAIN_LENGTH;
    let mut seed = [0u8; 32];
    box_node_link::handshake::fill_random(&mut seed)
        .map_err(|_| "cannot generate a bootstrap seed".to_owned())?;
    let chain_end = hex(&box_node_link::chain::hash_n(&seed, chain_length));

    let issued_at = now_seconds();
    let record = NodeKeyRecord::token(node_id, &chain_end, issued_at);
    collection.upsert(&[NodeKeyRecord {
        helper_version: helper_version.map(str::to_owned),
        ..record
    }])?;

    Ok(IssuedBootstrap {
        node_id: node_id.to_owned(),
        seed: hex(&seed),
        chain_length,
        chain_end,
        issued_at,
    })
}

/// Lowercase hex, for a seed or a chain value.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
/// Upgrade a node from its bootstrap mode to node-key mode.
///
/// The design has three modes and this is the transition between two of them
/// (section 6): a node enrols with a token chain, and its first successful
/// handshake can register the long-term key it will use from then on. After
/// the upgrade the chain is irrelevant -- the node proves itself with a
/// signature and the control plane checks it against the key it registered
/// here, which is what makes "a signature verified" mean "the right node
/// connected".
///
/// The signature is checked *before* the row is written. Upgrading first and
/// verifying later would let any caller register any key as any node.
///
/// A row that is already in node-key mode is left alone rather than
/// overwritten: a second registration attempt is either a bug or an attempt
/// to take the identity over, and both should be a refusal the caller can
/// see.
pub fn upgrade_to_node_key(
    collection: &Collection<NodeKeyRecord>,
    node_id: &str,
    pubkey_hex: &str,
    signature: &[u8],
    transcript: &[u8],
) -> BoxResult<NodeKeyRecord> {
    let rows = collection.load_all()?;
    let record = rows
        .iter()
        .find(|row| row.id == node_id)
        .ok_or_else(|| format!("no node key row for {node_id}"))?;

    // Already upgraded. The existing key stands; a caller offering a
    // different one is trying to become somebody else.
    if record.mode == box_node_link::node_keys::KeyMode::NodeKey {
        return Err(format!(
            "node {node_id} is already registered with a long-term key"
        ));
    }

    if !box_node_link::handshake::verify_node_key(pubkey_hex, transcript, signature) {
        return Err("the offered node key did not sign the transcript".to_owned());
    }

    let mut upgraded = record.clone();
    upgraded.mode = box_node_link::node_keys::KeyMode::NodeKey;
    upgraded.node_pubkey = Some(pubkey_hex.to_owned());
    // The chain is retired with the mode. Keeping it would leave a usable
    // credential in a row whose mode says it does not need one.
    upgraded.chain_end = None;
    upgraded.last_accepted = None;
    upgraded.secret = String::new();
    collection.upsert(&[upgraded.clone()])?;
    Ok(upgraded)
}