use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

use jiff::Timestamp;
use quant_core::MediaRef;
use quant_crypto::MediaSealer;
use quant_crypto::VaultKey;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::Result;
use crate::Store;
use crate::StoreError;
use crate::micros;

/// Sealed media files, one per distinct content hash:
/// `media/<first two hex chars>/<hash>`.
pub struct MediaStore {
	dir: PathBuf,
	sealer: MediaSealer,
}

fn valid_hash(hash: &str) -> bool {
	hash.len() == 64
		&& hash
			.bytes()
			.all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl MediaStore {
	/// Media under `dir`, sealed with the vault's media key.
	pub fn new(dir: PathBuf, key: &VaultKey) -> Self {
		Self {
			dir,
			sealer: MediaSealer::new(key),
		}
	}

	fn path(&self, hash: &str) -> Result<PathBuf> {
		if !valid_hash(hash) {
			return Err(StoreError::Invalid("media hash".into()));
		}
		Ok(self.dir.join(&hash[..2]).join(hash))
	}

	/// Seal and store `bytes`. Storing the same bytes twice writes once.
	pub fn put(&self, bytes: &[u8], mime: &str) -> Result<MediaRef> {
		let media = MediaRef::of(bytes, mime);
		let path = self.path(&media.hash)?;
		if path.exists() {
			return Ok(media);
		}
		let parent = path.parent().expect("media paths have a parent");
		fs::create_dir_all(parent)?;
		let sealed = self.sealer.seal(&media.hash, bytes);
		let temp = parent.join(format!(".{}.tmp", media.hash));
		{
			let mut file = fs::File::create(&temp)?;
			file.write_all(&sealed)?;
			file.sync_all()?;
		}
		fs::rename(&temp, &path)?;
		Ok(media)
	}

	/// Open stored media.
	pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
		let sealed = match fs::read(self.path(hash)?) {
			Ok(sealed) => sealed,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
			Err(e) => return Err(e.into()),
		};
		Ok(self.sealer.open(hash, &sealed)?)
	}

	/// Open media with a hard allocation limit before reading or decrypting it.
	/// The encrypted envelope's fixed overhead is allowed in addition to
	/// `max_bytes`.
	pub fn get_bounded(&self, hash: &str, max_bytes: usize) -> Result<Vec<u8>> {
		let file = match fs::File::open(self.path(hash)?) {
			Ok(file) => file,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Err(StoreError::NotFound);
			}
			Err(error) => return Err(error.into()),
		};
		let cap = max_bytes.saturating_add(quant_crypto::SEALED_OVERHEAD);
		let too_large = || StoreError::Invalid("media exceeds the phone transfer limit".into());
		if file.metadata()?.len() > u64::try_from(cap).unwrap_or(u64::MAX) {
			return Err(too_large());
		}
		let mut sealed = Vec::new();
		file.take(u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX))
			.read_to_end(&mut sealed)?;
		if sealed.len() > cap {
			return Err(too_large());
		}
		let plain = self.sealer.open(hash, &sealed)?;
		if plain.len() > max_bytes {
			return Err(too_large());
		}
		Ok(plain)
	}

	/// Delete stored media. Missing files are fine.
	pub fn remove(&self, hash: &str) -> Result<()> {
		match fs::remove_file(self.path(hash)?) {
			Ok(()) => Ok(()),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(e) => Err(e.into()),
		}
	}
}

impl Store {
	/// Remember that media was stored and when it should be swept.
	pub fn note_media(&self, media: &MediaRef, expires: Option<Timestamp>) -> Result<()> {
		self.conn().execute(
			"INSERT INTO media (hash, mime, size, stored_us, expires_us) VALUES (?1, ?2, ?3, ?4, \
			 ?5)
			 ON CONFLICT (hash) DO UPDATE SET expires_us = CASE
				WHEN excluded.expires_us IS NULL OR media.expires_us IS NULL THEN NULL
				ELSE max(media.expires_us, excluded.expires_us) END",
			params![
				media.hash,
				media.mime,
				i64::try_from(media.size).unwrap_or(i64::MAX),
				micros(Timestamp::now()),
				expires.map(micros),
			],
		)?;
		Ok(())
	}

	/// Whether media with `hash` is still held.
	pub fn has_media(&self, hash: &str) -> Result<bool> {
		Ok(self
			.conn()
			.query_row("SELECT 1 FROM media WHERE hash = ?1", [hash], |_| Ok(()))
			.optional()?
			.is_some())
	}

	/// The media type of held media.
	pub fn media_mime(&self, hash: &str) -> Result<Option<String>> {
		Ok(self
			.conn()
			.query_row("SELECT mime FROM media WHERE hash = ?1", [hash], |r| {
				r.get(0)
			})
			.optional()?)
	}

	/// Hashes of media past its expiry at `now`.
	pub fn expired_media(&self, now: Timestamp) -> Result<Vec<String>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(
			"SELECT hash FROM media WHERE expires_us IS NOT NULL AND expires_us <= ?1 LIMIT 10000",
		)?;
		let rows = statement.query_map([micros(now)], |r| r.get(0))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Set or clear when media expires. Clearing it is how a saved clip
	/// outlives the buffer chunks it was joined from, even if a chunk's
	/// content was identical.
	pub fn set_media_expiry(&self, hash: &str, expires: Option<Timestamp>) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE media SET expires_us = ?2 WHERE hash = ?1",
			params![hash, expires.map(micros)],
		)?;
		if changed == 0 {
			return Err(StoreError::NotFound);
		}
		Ok(())
	}

	/// Forget media after its file is gone. Records keep their reference, so
	/// agents see that something was captured even once the pixels are gone.
	pub fn forget_media(&self, hash: &str) -> Result<()> {
		self.conn()
			.execute("DELETE FROM media WHERE hash = ?1", [hash])?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;

	use super::*;

	#[test]
	fn media_is_sealed_on_disk_and_reads_back() {
		let dir = tempfile::tempdir().unwrap();
		let key = VaultKey::generate();
		let media = MediaStore::new(dir.path().to_path_buf(), &key);
		let bytes = b"pretend screenshot with a secret on it";
		let reference = media.put(bytes, "image/jpeg").unwrap();
		assert_eq!(media.get(&reference.hash).unwrap(), bytes);

		let on_disk =
			fs::read(dir.path().join(&reference.hash[..2]).join(&reference.hash)).unwrap();
		assert!(!on_disk.windows(6).any(|w| w == b"secret"));

		let stranger = MediaStore::new(dir.path().to_path_buf(), &VaultKey::generate());
		assert!(stranger.get(&reference.hash).is_err());

		media.remove(&reference.hash).unwrap();
		assert!(matches!(
			media.get(&reference.hash),
			Err(StoreError::NotFound)
		));
		assert!(media.get("../../etc/passwd").is_err());
	}

	#[test]
	fn expiry_keeps_the_longest_retention() {
		let store = crate::test_support::store();
		let now = Timestamp::now();
		let media = MediaRef::of(b"x", "image/jpeg");
		store
			.note_media(&media, Some(now + SignedDuration::from_hours(1)))
			.unwrap();
		store
			.note_media(&media, Some(now + SignedDuration::from_hours(5)))
			.unwrap();
		assert!(
			store
				.expired_media(now + SignedDuration::from_hours(2))
				.unwrap()
				.is_empty()
		);
		assert_eq!(
			store
				.expired_media(now + SignedDuration::from_hours(6))
				.unwrap(),
			vec![media.hash.clone()]
		);
		store.note_media(&media, None).unwrap();
		assert!(
			store
				.expired_media(now + SignedDuration::from_hours(100))
				.unwrap()
				.is_empty()
		);
		store.forget_media(&media.hash).unwrap();
		assert!(!store.has_media(&media.hash).unwrap());
	}

	#[test]
	fn a_cleared_expiry_survives_a_later_note() {
		let store = crate::test_support::store();
		let now = Timestamp::now();
		let media = MediaRef::of(b"clip", "audio/mp4");
		store
			.note_media(&media, Some(now + SignedDuration::from_hours(1)))
			.unwrap();
		// A saved clip clears the expiry outright, rather than keeping the
		// longest retention a buffer chunk set.
		store.set_media_expiry(&media.hash, None).unwrap();
		assert!(
			store
				.expired_media(now + SignedDuration::from_hours(24))
				.unwrap()
				.is_empty()
		);
		assert!(matches!(
			store.set_media_expiry("ff".repeat(32).as_str(), None),
			Err(StoreError::NotFound)
		));
	}
}

#[cfg(test)]
mod bounded_tests {
	use super::*;

	#[test]
	fn bounded_media_reads_check_the_envelope_before_decrypting() {
		let dir = tempfile::tempdir().unwrap();
		let media = MediaStore::new(dir.path().to_path_buf(), &VaultKey::generate());
		let reference = media.put(b"private audio", "audio/mp4").unwrap();
		assert!(matches!(
			media.get_bounded(&reference.hash, 1),
			Err(StoreError::Invalid(_))
		));
		assert_eq!(
			media.get_bounded(&reference.hash, 13).unwrap(),
			b"private audio"
		);
		let path = media.path(&reference.hash).unwrap();
		fs::OpenOptions::new()
			.write(true)
			.open(path)
			.unwrap()
			.set_len(1024 * 1024 * 1024)
			.unwrap();
		assert!(matches!(
			media.get_bounded(&reference.hash, 1024),
			Err(StoreError::Invalid(_))
		));
	}
}
