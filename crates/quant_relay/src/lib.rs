//! The relay: what a daemon, a client, and a relay server agree on.
//!
//! A vault stays on the owner's machine. `quantd` keeps one WebSocket open to
//! a relay; phones and agents open another; the relay joins the two and copies
//! bytes. TLS runs end to end between client and daemon *inside* that pipe,
//! so the relay sees only ciphertext. What is here is the part every side
//! must agree on:
//!
//! - how a daemon derives its rendezvous key and id from the vault key
//!   ([`key`]),
//! - the challenge-and-signature handshake and the messages on the wire
//!   ([`proto`]),
//! - the limits a relay enforces per rendezvous id ([`limits`]),
//! - the relay parameters a pairing link carries ([`link`]).
//!
//! Behind the `dial` feature, [`dial`] connects a WebSocket and wraps it as a
//! byte stream; behind the `server` feature, [`server`] is the relay itself:
//! the same axum routes `apps/relay` serves, over any [`server::RelayStore`],
//! with an in-memory store for tests. Both are on by default so the
//! workspace's crates and tests see one feature set; `apps/relay` turns them
//! off and takes only the protocol.

pub mod key;
pub mod limits;
pub mod link;
pub mod proto;
pub mod store;
pub mod url;

#[cfg(feature = "dial")]
pub mod dial;
#[cfg(feature = "server")]
pub mod server;

pub use key::RelayKey;
pub use key::rid;
pub use limits::Limits;
pub use url::RelayUrl;
