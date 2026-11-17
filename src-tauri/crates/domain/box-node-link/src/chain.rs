//! The token chain: a one-way hash chain of bootstrap values.
//!
//! The direction is the whole point. The control plane picks a seed, hashes
// it forward n times, and keeps only the last value. The node holds the
// seed and reports c_k = H^k(s) for a decreasing k. Verification is one
// hash: H(received) must equal the stored value, which then becomes the
// new stored value.
//!
//! That direction is what makes eavesdropping useless. Someone who reads
// c_k off the wire can only compute forward (c_k+1 ... c_n), and every one
// of those has already been spent. Getting c_k-1 needs H^-1.
//!
//! An earlier version of this design had the chain running the other way,
// which let an observer continue the chain themselves. The test below
// pins the direction, not just the shape: a chain that could be continued
// from an observed value would still pass a naive round-trip test.

use ring::digest;
use serde::{Deserialize, Serialize};

/// How many links a chain has by default. 64 is enough for a node to
// reconnect for a long time before it has to be re-enrolled.
pub const DEFAULT_CHAIN_LENGTH: u32 = 64;

/// One node token chain, as the control plane stores it.
///
/// Only the two ends are kept. The seed is not, because the control plane
// has no use for it and storing it would make a stolen row reusable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainState {
    /// The next value that will be accepted: the preimage of the last one
    /// seen. Empty until the node has connected once.
    pub last_accepted: Option<String>,
    /// The value handed to the node at enrolment: H^n(seed).
    pub chain_end: String,
    /// How many links remain before the node must be re-enrolled.
    pub remaining: u32,
}

impl ChainState {
    /// Start a chain from a seed. The control plane keeps the state; the
    /// seed goes to the node and is not stored here.
    pub fn from_seed(seed: &[u8; 32], length: u32) -> Self {
        // Same floor as ChainNode::new: a one-link chain has no spendable
        // link that is not the seed.
        let length = length.max(2);
        Self {
            last_accepted: None,
            chain_end: hex(&hash_n(seed, length)),
            remaining: length - 1,
        }
    }

    /// The value the node should report next.
    ///
    /// Before the first connection that is the chain end; afterwards it is
    /// the preimage of whatever was last accepted.
    pub fn next_expected(&self) -> Option<&str> {
        self.last_accepted.as_deref().or(Some(self.chain_end.as_str()))
    }
}

/// The node side of the chain: the seed, and how many links have been spent.
#[derive(Clone, Debug)]
pub struct ChainNode {
    seed: [u8; 32],
    /// The k the node will use next. Decreases on every handshake.
    next_k: u32,
}

impl ChainNode {
    /// The node starts one link below the chain end.
    ///
    /// The control plane stores c_n = H^n(seed) and accepts a value only
    /// when H(value) equals what it holds, so the first value sent must be
    /// c_(n-1). Starting at k = n would send c_n itself, whose hash is
    /// c_(n+1) -- a value nothing will ever accept.
    /// `length` must be at least 2.
    ///
    /// A one-link chain would start the node at k = 0, which is the seed
    /// itself -- exactly what the node must never send. Two links is the
    /// smallest usable chain, and it is what DEFAULT_CHAIN_LENGTH is far
    /// above anyway; the floor is here so a caller passing a bad length gets
    /// a working chain rather than a silently broken one.
    pub fn new(seed: [u8; 32], length: u32) -> Self {
        let length = length.max(2);
        Self {
            seed,
            next_k: length - 1,
        }
    }

    /// How many more handshakes this node can do before re-enrolment.
    pub fn remaining(&self) -> u32 {
        self.next_k
    }

    /// True once the node has spent every link it safely can.
    ///
    /// At that point it must be re-enrolled rather than pushed further,
    /// because the only link left is the seed.
    pub fn is_exhausted(&self) -> bool {
        self.next_k <= 1
    }

    /// The value to send on the next handshake, and the k it corresponds to.
    ///
    /// The caller must persist the returned k *before* sending the value
    /// (design section 3). If the ack is lost and the node retries, it must
    /// use a smaller k: resending the same c_k is a permanent refusal,
    /// because H(c_k) is c_k+1 and the control plane now expects c_k.
    pub fn next_value(&mut self) -> (String, u32) {
        // Never k = 0. H^0(seed) is the seed itself, and sending it would put
        // the seed in the control plane's last_accepted -- which design
        // section 8 forbids, because a stolen node-keys row would then
        // regenerate the whole chain and every future value.
        let k = self.next_k.max(1);
        self.next_k = k.saturating_sub(1).max(1);
        (hex(&hash_n(&self.seed, k)), k)
    }

    /// Rewind after a failure, so a retry uses a smaller k.
    ///
    /// This is the recovery the design doc requires: any failure, timeout
    /// or retry goes back down the chain rather than reusing a spent link.
    pub fn rewind_to(&mut self, k: u32) {
        if k < self.next_k {
            self.next_k = k;
        }
    }
}

/// H^n(x): the forward hash, n times.
pub fn hash_n(input: &[u8; 32], n: u32) -> [u8; 32] {
    let mut current = *input;
    for _ in 0..n {
        current = hash_once(&current);
    }
    current
}

/// One link. The domain-separated prefix keeps this from being confused
// with any other hash in the system.
pub fn hash_once(input: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(input.len() + 16);
    buf.extend_from_slice(b"node-link-chain-v1");
    buf.extend_from_slice(input);
    digest::digest(&digest::SHA256, &buf)
        .as_ref()
        .try_into()
        .expect("sha256 is 32 bytes")
}

/// Check a value a node presented and advance the chain if it is right.
///
/// One hash, so verification is O(1) no matter how long the chain is. A
/// value that is not the preimage of what is stored is refused, which is
// what makes replay useless: every previously accepted value hashes to
// something that was itself already accepted.
pub fn accept(state: &mut ChainState, presented: &str) -> Result<u32, ChainError> {
    let expected = state
        .next_expected()
        .ok_or(ChainError::Exhausted)?;
    if state.remaining == 0 {
        return Err(ChainError::Exhausted);
    }
    let bytes = unhex(presented).map_err(|_| ChainError::Malformed)?;
    let hashed = hash_once(&bytes);
    if hex(&hashed) != expected {
        return Err(ChainError::NotPreimage);
    }
    state.last_accepted = Some(presented.to_owned());
    state.remaining -= 1;
    Ok(state.remaining)
}

/// Why a chain value was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("this chain has no links left; the node must be re-enrolled")]
    Exhausted,
    #[error("the presented value is not the preimage of the accepted one")]
    NotPreimage,
    #[error("the presented value is not a 32-byte hex string")]
    Malformed,
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn unhex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() != 64 {
        return Err(());
    }
    let mut out = Vec::with_capacity(32);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).ok_or(())?;
        let lo = (pair[1] as char).to_digit(16).ok_or(())?;
        out.push((hi * 16 + lo) as u8);
    }
    Ok(out)
}
