//! Who may see what, how much, and a record of every time they did.
//!
//! A Quant vault holds a life's worth of screens, conversations, and heart
//! beats, so nothing reads it by default. Each client (an agent, the CLI, a
//! phone) holds its own signing key and is paired with a [`Grant`] the owner
//! approved. A grant names [`Scope`]s in the style of GitHub's fine-grained
//! permissions (`health:summary`, `screen:read`, `audio:raw`), how far back it
//! may look, and [`Limits`] on how much it may pull out. Every request is
//! signed ([`auth`]), checked, rate limited ([`Limiter`]), its text redacted
//! ([`redact`]), and written to a hash-chained [`audit`] log.

pub mod audit;
pub mod auth;
mod device;
mod grant;
mod limiter;
pub mod redact;
mod scope;

pub use device::*;
pub use grant::*;
pub use limiter::*;
pub use scope::*;

/// Errors from parsing or checking access values.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccessError {
	/// A scope string did not parse.
	#[error(
		"invalid scope `{0}`: expected `<family|stream|*>:<summary|read|raw|write>` or \
		 `audit:read`"
	)]
	InvalidScope(String),
	/// A public key did not parse.
	#[error("invalid public key")]
	InvalidKey,
	/// A request could not be authenticated.
	#[error("unauthenticated: {0}")]
	Unauthenticated(String),
}
