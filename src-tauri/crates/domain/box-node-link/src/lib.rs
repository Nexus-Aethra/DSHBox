//! Frame codec for the node link: one length-prefixed, type-tagged JSON
//! frame at a time.
//!
//! This crate is the *transport* half of the node link and nothing else. It
//! knows how a frame is laid out on the wire and how to refuse a malformed
//! one; it does not know about sessions, tokens, crypto or tasks. Those
//! belong to the layers above (L2 handshake, L4 task streaming) and arrive
//! later without changing this file.
//!
//! Both sides of the link use it -- `dshboxd` reads requests and writes
//! responses, `box-helper` writes requests and reads responses -- which is
//! why it is a crate and not a module inside either binary.

pub mod frame;
