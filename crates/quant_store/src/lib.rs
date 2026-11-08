//! The Quant vault on disk.
//!
//! A vault is a directory:
//!
//! ```text
//! vault.json   manifest: vault id and the passphrase-wrapped vault key
//! quant.db     SQLCipher database: records, series, grants, audit
//! media/       sealed screenshots and audio, named by content hash
//! ```
//!
//! [`Vault`] creates and opens that directory; [`Store`] is the open
//! database. The store applies no permissions of its own: the daemon checks a
//! grant before every call and passes only what the grant allows.

mod access;
mod audit;
mod claims;
mod embeddings;
mod media;
mod records;
mod schema;
mod series;
mod vault;

use std::path::Path;
use std::sync::Mutex;
use std::sync::MutexGuard;

pub use access::*;
pub use claims::*;
pub use embeddings::*;
use jiff::Timestamp;
pub use media::*;
use quant_core::Family;
use quant_core::StreamId;
use quant_crypto::VaultKey;
pub use records::*;
use rusqlite::Connection;
pub use series::*;
pub use vault::*;

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
	/// SQLite failed.
	#[error("database: {0}")]
	Sqlite(#[from] rusqlite::Error),
	/// The key did not open the database.
	#[error("the vault key does not open this database")]
	WrongKey,
	/// A file operation failed.
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
	/// A stored value did not decode.
	#[error("corrupt value in {0}")]
	Corrupt(&'static str),
	/// Something that should exist does not.
	#[error("not found")]
	NotFound,
	/// A value was rejected.
	#[error("invalid: {0}")]
	Invalid(String),
	/// Key handling failed.
	#[error(transparent)]
	Crypto(#[from] quant_crypto::CryptoError),
	/// JSON failed.
	#[error("json: {0}")]
	Json(#[from] serde_json::Error),
}

/// A result from the store.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Which streams a query covers: the union of whole families and single
/// streams (with their children). Built by the daemon from what a grant
/// allows, so the store never returns a stream outside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
	/// Every stream.
	pub all: bool,
	/// Whole families.
	pub families: Vec<Family>,
	/// Single streams and their children.
	pub streams: Vec<StreamId>,
}

impl Selector {
	/// Every stream.
	pub fn all() -> Self {
		Self {
			all: true,
			..Self::default()
		}
	}

	/// Exactly one stream (and its children).
	pub fn stream(stream: StreamId) -> Self {
		Self {
			streams: vec![stream],
			..Self::default()
		}
	}

	/// One whole family.
	pub fn family(family: Family) -> Self {
		Self {
			families: vec![family],
			..Self::default()
		}
	}

	/// Whether it selects nothing.
	pub fn is_empty(&self) -> bool {
		!self.all && self.families.is_empty() && self.streams.is_empty()
	}

	/// Whether `stream` is selected.
	pub fn selects(&self, stream: &StreamId) -> bool {
		self.all
			|| self.families.contains(&stream.family())
			|| self.streams.iter().any(|s| {
				stream.as_str() == s.as_str()
					|| stream
						.as_str()
						.strip_prefix(s.as_str())
						.is_some_and(|rest| rest.starts_with('.'))
			})
	}

	/// A SQL condition over the `stream` and `family` columns (prefixed with
	/// `prefix`, such as `"r."`), pushing its parameters onto `params`.
	pub(crate) fn sql(&self, prefix: &str, params: &mut Vec<rusqlite::types::Value>) -> String {
		if self.all {
			return "1".into();
		}
		if self.is_empty() {
			return "0".into();
		}
		let mut clauses = Vec::new();
		for family in &self.families {
			params.push(family.as_str().to_owned().into());
			clauses.push(format!("{prefix}family = ?"));
		}
		for stream in &self.streams {
			params.push(stream.as_str().to_owned().into());
			params.push(format!("{}.%", stream.as_str()).into());
			clauses.push(format!("({prefix}stream = ? OR {prefix}stream LIKE ?)"));
		}
		format!("({})", clauses.join(" OR "))
	}
}

/// An open vault database.
pub struct Store {
	conn: Mutex<Connection>,
}

pub(crate) fn micros(t: Timestamp) -> i64 {
	t.as_microsecond()
}

pub(crate) fn from_micros(us: i64) -> Result<Timestamp> {
	Timestamp::from_microsecond(us).map_err(|_| StoreError::Corrupt("timestamp"))
}

impl Store {
	/// Open (or create) the database at `path` with the vault key.
	pub fn open(path: &Path, key: &VaultKey) -> Result<Self> {
		let conn = Connection::open(path)?;
		Self::init(conn, key)
	}

	/// An in-memory, still encrypted store, for tests.
	pub fn open_in_memory(key: &VaultKey) -> Result<Self> {
		Self::init(Connection::open_in_memory()?, key)
	}

	fn init(conn: Connection, key: &VaultKey) -> Result<Self> {
		conn.execute_batch(&format!(
			"PRAGMA key = \"{}\";",
			key.sqlcipher_pragma().as_str()
		))?;
		// A wrong key only shows when the first page is read.
		if conn
			.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
				r.get::<_, i64>(0)
			})
			.is_err()
		{
			return Err(StoreError::WrongKey);
		}
		conn.execute_batch(
			"PRAGMA journal_mode = WAL;
			 PRAGMA synchronous = NORMAL;
			 PRAGMA foreign_keys = ON;
			 PRAGMA secure_delete = ON;
			 PRAGMA busy_timeout = 5000;",
		)?;
		schema::migrate(&conn)?;
		Ok(Self {
			conn: Mutex::new(conn),
		})
	}

	pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
		self.conn
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Run SQLite's integrity check.
	pub fn integrity_check(&self) -> Result<bool> {
		let ok: String = self
			.conn()
			.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
		Ok(ok == "ok")
	}

	/// Reclaim space after large deletions.
	pub fn vacuum(&self) -> Result<()> {
		let conn = self.conn();
		conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
		Ok(())
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use quant_crypto::VaultKey;

	use crate::Store;

	pub fn store() -> Store {
		Store::open_in_memory(&VaultKey::generate()).unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn selector_matches_families_and_stream_prefixes() {
		let selector = Selector {
			all: false,
			families: vec![Family::Health],
			streams: vec!["sensor.vest".parse().unwrap()],
		};
		for yes in ["health.heart_rate", "sensor.vest", "sensor.vest.ecg"] {
			assert!(selector.selects(&yes.parse().unwrap()), "{yes}");
		}
		for no in ["sensor.vestibular", "screen.frame"] {
			assert!(!selector.selects(&no.parse().unwrap()), "{no}");
		}
		assert!(Selector::default().is_empty());
	}

	#[test]
	fn a_database_cannot_be_opened_with_another_key() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("quant.db");
		let key = VaultKey::generate();
		drop(Store::open(&path, &key).unwrap());
		assert!(matches!(
			Store::open(&path, &VaultKey::generate()),
			Err(StoreError::WrongKey)
		));
		assert!(Store::open(&path, &key).unwrap().integrity_check().unwrap());
	}

	#[test]
	fn the_database_file_is_not_plaintext_sqlite() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("quant.db");
		let store = Store::open(&path, &VaultKey::generate()).unwrap();
		store
			.conn()
			.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
			.unwrap();
		drop(store);
		let bytes = std::fs::read(&path).unwrap();
		assert!(!bytes.starts_with(b"SQLite format 3"));
	}
}
