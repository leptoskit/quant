use rusqlite::Connection;

use crate::Result;

/// Each entry moves the schema up one version. Never edit a shipped entry;
/// append a new one.
const MIGRATIONS: &[&str] = &[
	// 1: records, series, media, clients, grants, audit.
	"
	CREATE TABLE records (
		rowid INTEGER PRIMARY KEY,
		id BLOB NOT NULL UNIQUE,
		stream TEXT NOT NULL,
		family TEXT NOT NULL,
		device BLOB NOT NULL,
		start_us INTEGER NOT NULL,
		end_us INTEGER,
		observed_us INTEGER NOT NULL,
		text TEXT,
		data TEXT,
		media_hash TEXT,
		media_mime TEXT,
		media_size INTEGER
	) STRICT;
	CREATE INDEX records_by_stream ON records (stream, start_us);
	CREATE INDEX records_by_family ON records (family, start_us);
	CREATE INDEX records_by_time ON records (start_us);
	CREATE INDEX records_by_media ON records (media_hash) WHERE media_hash IS NOT NULL;

	CREATE VIRTUAL TABLE records_fts USING fts5 (
		text,
		content = 'records',
		content_rowid = 'rowid',
		tokenize = 'unicode61 remove_diacritics 2'
	);
	CREATE TRIGGER records_fts_insert AFTER INSERT ON records WHEN new.text IS NOT NULL BEGIN
		INSERT INTO records_fts (rowid, text) VALUES (new.rowid, new.text);
	END;
	CREATE TRIGGER records_fts_delete AFTER DELETE ON records WHEN old.text IS NOT NULL BEGIN
		INSERT INTO records_fts (records_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
	END;
	CREATE TRIGGER records_fts_update AFTER UPDATE OF text ON records BEGIN
		INSERT INTO records_fts (records_fts, rowid, text)
			SELECT 'delete', old.rowid, old.text WHERE old.text IS NOT NULL;
		INSERT INTO records_fts (rowid, text)
			SELECT new.rowid, new.text WHERE new.text IS NOT NULL;
	END;

	CREATE TABLE series (
		rowid INTEGER PRIMARY KEY,
		hash BLOB NOT NULL UNIQUE,
		stream TEXT NOT NULL,
		family TEXT NOT NULL,
		device BLOB NOT NULL,
		start_us INTEGER NOT NULL,
		end_us INTEGER NOT NULL,
		rows INTEGER NOT NULL,
		channels TEXT NOT NULL,
		stats TEXT NOT NULL,
		body BLOB NOT NULL
	) STRICT;
	CREATE INDEX series_by_stream ON series (stream, start_us);
	CREATE INDEX series_by_family ON series (family, start_us);

	CREATE TABLE streams (
		id TEXT PRIMARY KEY,
		spec TEXT NOT NULL
	) STRICT;

	CREATE TABLE devices (
		id BLOB PRIMARY KEY,
		name TEXT NOT NULL,
		platform TEXT NOT NULL,
		first_seen_us INTEGER NOT NULL,
		last_seen_us INTEGER NOT NULL
	) STRICT;

	CREATE TABLE media (
		hash TEXT PRIMARY KEY,
		mime TEXT NOT NULL,
		size INTEGER NOT NULL,
		stored_us INTEGER NOT NULL,
		expires_us INTEGER
	) STRICT;
	CREATE INDEX media_by_expiry ON media (expires_us) WHERE expires_us IS NOT NULL;

	CREATE TABLE clients (
		id BLOB PRIMARY KEY,
		name TEXT NOT NULL,
		kind TEXT NOT NULL,
		public_key TEXT NOT NULL UNIQUE,
		status TEXT NOT NULL,
		requested TEXT NOT NULL,
		created_us INTEGER NOT NULL,
		last_seen_us INTEGER
	) STRICT;

	CREATE TABLE grants (
		id BLOB PRIMARY KEY,
		client BLOB NOT NULL REFERENCES clients (id),
		body TEXT NOT NULL,
		revoked INTEGER NOT NULL DEFAULT 0
	) STRICT;
	CREATE INDEX grants_by_client ON grants (client) WHERE revoked = 0;

	CREATE TABLE audit (
		seq INTEGER PRIMARY KEY,
		at_us INTEGER NOT NULL,
		actor TEXT NOT NULL,
		action TEXT NOT NULL,
		entry TEXT NOT NULL,
		hash TEXT NOT NULL
	) STRICT;
	CREATE INDEX audit_by_time ON audit (at_us);
	",
];

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
	let version: usize = conn
		.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?
		.try_into()
		.unwrap_or(0);
	for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
		let tx = conn.unchecked_transaction()?;
		tx.execute_batch(migration)?;
		tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
		tx.commit()?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn migrations_are_idempotent() {
		let conn = Connection::open_in_memory().unwrap();
		migrate(&conn).unwrap();
		migrate(&conn).unwrap();
		let version: i64 = conn
			.query_row("PRAGMA user_version", [], |r| r.get(0))
			.unwrap();
		assert_eq!(usize::try_from(version).unwrap(), MIGRATIONS.len());
	}
}
