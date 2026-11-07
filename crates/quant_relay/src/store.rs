//! What a relay persists, as the server sees it.
//!
//! Implementations live wherever the rows do: [`server::MemoryStore`] in
//! tests, `leptoskit_db` tables in `apps/relay`. The relay keeps everything
//! else (live sockets, counters) in memory.

use std::future::Future;

/// Errors a store can report.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
	/// The relay gates registration on invites and none was presented.
	#[error("this relay needs an invite")]
	InviteRequired,
	/// The invite is unknown or already used.
	#[error("invite unknown or already used")]
	InviteUsed,
	/// A different key already owns this rendezvous id.
	#[error("a different key owns this rendezvous id")]
	KeyMismatch,
	/// The store itself failed.
	#[error("{0}")]
	Failed(String),
}

/// What the relay persists: registrations, invites, and usage. Live sockets
/// and counters stay in memory; a restart simply asks daemons to listen
/// again.
pub trait RelayStore: Clone + Send + Sync + 'static {
	/// Register `rid` with `public_key`, consuming `invite` when the relay
	/// asks for one. A `rid` already registered must present the same key.
	fn register(
		&self,
		rid: &str,
		public_key: &str,
		invite: Option<&str>,
	) -> impl Future<Output = Result<(), StoreError>> + Send;

	/// Note that `rid` was seen just now.
	fn touch(&self, rid: &str) -> impl Future<Output = ()> + Send;

	/// Add `bytes` to `rid`'s counter for `day`.
	fn add_usage(&self, rid: &str, day: &str, bytes: u64) -> impl Future<Output = ()> + Send;

	/// How many bytes `rid` has used on `day`.
	fn usage(&self, rid: &str, day: &str) -> impl Future<Output = Result<u64, StoreError>> + Send;
}
