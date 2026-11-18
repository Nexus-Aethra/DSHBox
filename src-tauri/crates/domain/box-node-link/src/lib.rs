//! The node link: frame codec, handshake, record layer, and token chain.
//!
//! This crate is the *protocol* half of the node link and nothing else. It
//! knows how a frame is laid out on the wire, how a bootstrap secret becomes
//! two session keys, and how a token chain is verified; it does not know
//! about the daemon, the store, the scheduler, or Tauri.
//!
//! Both sides of the link use it -- dshboxd is the control plane,
//! box-helper is the node -- which is why it is a crate and not a module
//! inside either binary.
//!
//! The layering: frame.rs is the transport, handshake.rs is the key
//! agreement, session.rs is the record layer the handshake produces keys
//! for, and chain.rs is the token-chain bootstrap. Each is a separate file
//! because each changes for a different reason: a wire-format fix does not
//! touch the crypto, and a crypto fix does not touch the chain.

pub mod chain;
pub mod frame;
pub mod handshake;
pub mod kdf;

/// The system random source, re-exported so a caller can build a handshake
/// without depending on `ring` itself. The primitive and its only consumer
/// stay in the same crate.
pub use ring::rand::SystemRandom;
pub mod node_keys;
pub mod session;
pub mod session_io;
pub mod wire;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
