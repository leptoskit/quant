//! The relay's database: registrations, invites, and daily byte counts.
//!
//! What a relay may know about a vault is exactly what these tables hold:
//! its rendezvous id, its public key, when it was last seen, which invite it
//! used, and how many bytes flowed. No request bodies, no keys, no
//! addresses.
//!
//! After changing a table, run `leptoskit db migration create <name>` and
//! commit what it writes under `migrations/`.

use leptoskit_db::Id;
use leptoskit_db::Timestamp;
use leptoskit_db::table;

/// A daemon registered on this relay by rendezvous id.
#[table(name = "registrations")]
pub struct Registration {
	/// The row's id.
	pub id: Id<Registration>,
	/// The rendezvous id: lowercase base32, 26 characters.
	#[db(unique, min_len = 26, max_len = 26, regex = "^[a-z2-7]+$")]
	pub rid: String,
	/// The daemon's relay key, as unpadded base64. It derives the rid.
	#[db(min_len = 43, max_len = 43)]
	pub public_key: String,
	/// When the daemon first registered.
	#[db(created_at)]
	pub created_at: Timestamp,
	/// When the daemon was last seen.
	pub last_seen: Timestamp,
	/// The invite that opened the door, when the relay asked for one.
	pub invite_id: Option<Id<Invite>>,
}

/// An invite code, spent once.
#[table(name = "invites")]
pub struct Invite {
	/// The row's id.
	pub id: Id<Invite>,
	/// The code a daemon presents on its first listen.
	#[db(unique, min_len = 8, max_len = 64)]
	pub code: String,
	/// The rendezvous id that spent it.
	pub used_by: Option<String>,
	/// When it was spent.
	pub used_at: Option<Timestamp>,
	/// When it was created.
	#[db(created_at)]
	pub created_at: Timestamp,
}

/// Bytes moved for one rendezvous id on one UTC day, both directions.
#[table(name = "usage")]
pub struct Usage {
	/// The row's id.
	pub id: Id<Usage>,
	/// `<rid>/<day>`, so each pair has exactly one row.
	#[db(unique)]
	pub usage_key: String,
	/// The rendezvous id the bytes flowed for.
	#[db(index)]
	pub rid: String,
	/// The UTC day, `YYYY-MM-DD`.
	pub day: String,
	/// The count, both directions added.
	pub bytes: i64,
	/// When the counter last moved.
	#[db(updated_at)]
	pub updated_at: Timestamp,
}

leptoskit_db::export_schema!();

/// Storage for the relay's protocol: registrations, invites, and usage in
/// `leptoskit_db`.
#[cfg(feature = "server")]
mod store;
#[cfg(feature = "server")]
pub use store::RelayDb;

/// Every migration, embedded for the server to apply at startup.
#[cfg(feature = "server")]
pub static MIGRATIONS: &[leptoskit_db::Migration] = leptoskit_db::migrations!("migrations");
