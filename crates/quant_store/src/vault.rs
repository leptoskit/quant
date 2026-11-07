use std::fs;
use std::path::Path;
use std::path::PathBuf;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::MediaRef;
use quant_crypto::KdfParams;
use quant_crypto::VaultKey;
use quant_crypto::WrappedKey;
use serde::Deserialize;
use serde::Serialize;

use crate::MediaStore;
use crate::Result;
use crate::Store;
use crate::StoreError;

/// The vault's manifest, `vault.json`. It holds nothing secret in the clear.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
	/// Manifest format.
	pub format: u32,
	/// The vault's id, which names its keychain entry.
	pub vault_id: String,
	/// When it was created.
	pub created_at: Timestamp,
	/// The vault key, sealed under the owner's passphrase.
	pub owner_key: WrappedKey,
	/// Fingerprint of the vault key, to check a keychain copy matches.
	pub key_fingerprint: String,
	/// The id of the device the vault lives on.
	pub device: DeviceId,
}

/// Where a vault's files live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultPaths {
	/// The vault directory.
	pub root: PathBuf,
}

impl VaultPaths {
	/// Paths under `root`.
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into() }
	}

	/// `$QUANT_HOME`, or the platform's per-user data directory.
	pub fn default_root() -> PathBuf {
		quant_core::paths::default_vault_root()
	}

	/// `vault.json`.
	pub fn manifest(&self) -> PathBuf {
		self.root.join("vault.json")
	}

	/// The database.
	pub fn db(&self) -> PathBuf {
		self.root.join("quant.db")
	}

	/// The media directory.
	pub fn media(&self) -> PathBuf {
		self.root.join("media")
	}

	/// The daemon's socket; see [`quant_core::paths::socket_path`].
	pub fn socket(&self) -> PathBuf {
		quant_core::paths::socket_path(&self.root)
	}

	/// Make sure the socket's directory exists and only this user can use it.
	/// The daemon calls this before binding; clients call it before
	/// connecting, so nobody else can stand up a fake daemon to collect the
	/// owner's passphrase.
	pub fn secure_socket_dir(&self) -> Result<()> {
		quant_core::paths::secure_socket_dir(&self.root).map_err(|error| {
			if error.kind() == std::io::ErrorKind::PermissionDenied {
				StoreError::Invalid(error.to_string())
			} else {
				StoreError::Io(error)
			}
		})
	}

	/// Whether a vault exists here.
	pub fn exists(&self) -> bool {
		self.manifest().exists()
	}
}

/// An open vault: its database, its media, and its key.
pub struct Vault {
	/// Where it lives.
	pub paths: VaultPaths,
	/// Its manifest.
	pub manifest: Manifest,
	/// The database.
	pub store: Store,
	/// Sealed media.
	pub media: MediaStore,
	key: VaultKey,
}

fn private_dir(path: &Path) -> Result<()> {
	fs::create_dir_all(path)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
	}
	Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
	let temp = path.with_extension("tmp");
	fs::write(&temp, bytes)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
	}
	fs::rename(temp, path)?;
	Ok(())
}

impl Vault {
	/// Create a vault at `paths`, sealing a new key under `passphrase`.
	pub fn create(paths: VaultPaths, passphrase: &str, kdf: KdfParams) -> Result<Self> {
		if paths.exists() {
			return Err(StoreError::Invalid(format!(
				"a vault already exists at {}",
				paths.root.display()
			)));
		}
		private_dir(&paths.root)?;
		let key = VaultKey::generate();
		let manifest = Manifest {
			format: 1,
			vault_id: uuid::Uuid::now_v7().simple().to_string(),
			created_at: Timestamp::now(),
			owner_key: WrappedKey::wrap(&key, passphrase, kdf)?,
			key_fingerprint: key.fingerprint(),
			device: DeviceId::new(),
		};
		write_private(&paths.manifest(), &serde_json::to_vec_pretty(&manifest)?)?;
		Self::open_with(paths, manifest, key)
	}

	/// Read a vault's manifest.
	pub fn read_manifest(paths: &VaultPaths) -> Result<Manifest> {
		let bytes = match fs::read(paths.manifest()) {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
			Err(e) => return Err(e.into()),
		};
		Ok(serde_json::from_slice(&bytes)?)
	}

	/// Open a vault with its key.
	pub fn open(paths: VaultPaths, key: VaultKey) -> Result<Self> {
		let manifest = Self::read_manifest(&paths)?;
		if manifest.key_fingerprint != key.fingerprint() {
			return Err(StoreError::WrongKey);
		}
		Self::open_with(paths, manifest, key)
	}

	/// Open a vault with the owner's passphrase.
	pub fn unlock(paths: VaultPaths, passphrase: &str) -> Result<Self> {
		let manifest = Self::read_manifest(&paths)?;
		let key = manifest.owner_key.unwrap(passphrase)?;
		Self::open_with(paths, manifest, key)
	}

	fn open_with(paths: VaultPaths, manifest: Manifest, key: VaultKey) -> Result<Self> {
		private_dir(&paths.media())?;
		let store = Store::open(&paths.db(), &key)?;
		let media = MediaStore::new(paths.media(), &key);
		Ok(Self {
			paths,
			manifest,
			store,
			media,
			key,
		})
	}

	/// The vault key, for saving to the keychain.
	pub fn key(&self) -> &VaultKey {
		&self.key
	}

	/// Whether `passphrase` is the owner's.
	pub fn check_passphrase(&self, passphrase: &str) -> bool {
		self.manifest
			.owner_key
			.unwrap(passphrase)
			.is_ok_and(|key| key == self.key)
	}

	/// Change the owner's passphrase. The vault key, and so the data, stay
	/// the same.
	pub fn change_passphrase(&mut self, old: &str, new: &str, kdf: KdfParams) -> Result<()> {
		if !self.check_passphrase(old) {
			return Err(quant_crypto::CryptoError::WrongPassphrase.into());
		}
		let mut manifest = self.manifest.clone();
		manifest.owner_key = WrappedKey::wrap(&self.key, new, kdf)?;
		write_private(
			&self.paths.manifest(),
			&serde_json::to_vec_pretty(&manifest)?,
		)?;
		self.manifest = manifest;
		Ok(())
	}

	/// Store media for a record, kept for `retention_hours` (forever when
	/// `None`).
	pub fn put_media(
		&self,
		bytes: &[u8],
		mime: &str,
		retention_hours: Option<u32>,
	) -> Result<MediaRef> {
		let media = self.media.put(bytes, mime)?;
		let expires = retention_hours
			.map(|hours| Timestamp::now() + SignedDuration::from_hours(i64::from(hours)));
		self.store.note_media(&media, expires)?;
		Ok(media)
	}

	/// Open stored media, if it is still held.
	pub fn get_media(&self, hash: &str) -> Result<Vec<u8>> {
		if !self.store.has_media(hash)? {
			return Err(StoreError::NotFound);
		}
		self.media.get(hash)
	}

	/// Delete media past its retention. Returns how many files went.
	pub fn sweep_media(&self, now: Timestamp) -> Result<usize> {
		let expired = self.store.expired_media(now)?;
		for hash in &expired {
			self.media.remove(hash)?;
			self.store.forget_media(hash)?;
		}
		Ok(expired.len())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const PASS: &str = "correct horse battery staple";

	#[test]
	fn creates_unlocks_and_reopens() {
		let dir = tempfile::tempdir().unwrap();
		let paths = VaultPaths::new(dir.path().join("vault"));
		let vault = Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap();
		let key = vault.key().clone();
		assert!(vault.check_passphrase(PASS));
		assert!(!vault.check_passphrase("not the passphrase"));
		drop(vault);

		assert!(Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).is_err());
		assert!(Vault::unlock(paths.clone(), "not the passphrase").is_err());
		assert!(Vault::unlock(paths.clone(), PASS).is_ok());
		assert!(matches!(
			Vault::open(paths.clone(), VaultKey::generate()),
			Err(StoreError::WrongKey)
		));
		assert!(Vault::open(paths, key).is_ok());

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mode = fs::metadata(dir.path().join("vault"))
				.unwrap()
				.permissions()
				.mode();
			assert_eq!(mode & 0o777, 0o700);
		}
	}

	#[test]
	fn long_vault_paths_get_a_short_private_socket() {
		let short = VaultPaths::new("/v");
		assert_eq!(short.socket(), PathBuf::from("/v/quant.sock"));
		let long = VaultPaths::new(format!("/{}", "deep/".repeat(30)));
		let socket = long.socket();
		assert_eq!(socket, quant_core::paths::socket_path(&long.root));
		let dir = socket.parent().unwrap();
		assert_ne!(dir, long.root);
		long.secure_socket_dir().unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mode = fs::metadata(dir).unwrap().permissions().mode();
			assert_eq!(mode & 0o777, 0o700);
		}
	}

	#[test]
	fn changing_the_passphrase_keeps_the_data() {
		let dir = tempfile::tempdir().unwrap();
		let paths = VaultPaths::new(dir.path());
		let mut vault = Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap();
		let media = vault.put_media(b"clip", "audio/wav", None).unwrap();
		vault
			.change_passphrase(PASS, "a brand new passphrase", KdfParams::INSECURE_FAST)
			.unwrap();
		drop(vault);
		assert!(Vault::unlock(paths.clone(), PASS).is_err());
		let vault = Vault::unlock(paths, "a brand new passphrase").unwrap();
		assert_eq!(vault.get_media(&media.hash).unwrap(), b"clip");
	}

	#[test]
	fn sweeps_expired_media() {
		let dir = tempfile::tempdir().unwrap();
		let vault =
			Vault::create(VaultPaths::new(dir.path()), PASS, KdfParams::INSECURE_FAST).unwrap();
		let short = vault.put_media(b"audio", "audio/wav", Some(1)).unwrap();
		let kept = vault.put_media(b"note", "text/plain", None).unwrap();
		let later = Timestamp::now() + SignedDuration::from_hours(2);
		assert_eq!(vault.sweep_media(later).unwrap(), 1);
		assert!(matches!(
			vault.get_media(&short.hash),
			Err(StoreError::NotFound)
		));
		assert_eq!(vault.get_media(&kept.hash).unwrap(), b"note");
	}
}
