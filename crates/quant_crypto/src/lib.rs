//! Keys and sealing for a Quant vault.
//!
//! A vault has one random 256-bit [`VaultKey`]. It never touches disk in the
//! clear. Two wrapped copies exist:
//!
//! - one sealed under the owner's passphrase with Argon2id ([`WrappedKey`]),
//!   which recovers the vault on a new machine and proves owner presence for
//!   admin actions such as approving a grant, and
//! - one in the OS keychain ([`keychain`]), so the capture daemon can open the
//!   vault at login without asking.
//!
//! Subkeys for the database and for media are derived from the vault key, so
//! one never stands in for the other.

#[cfg(feature = "keychain")]
pub mod keychain;
mod seal;
mod wrap;

pub use seal::*;
pub use wrap::*;
use zeroize::Zeroize;
use zeroize::ZeroizeOnDrop;

/// Errors from key handling.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
	/// The passphrase did not unlock the key.
	#[error("wrong passphrase")]
	WrongPassphrase,
	/// Sealed data failed authentication: it was altered or the key is wrong.
	#[error("sealed data failed authentication")]
	Tampered,
	/// Sealed data was too short or had an unknown version.
	#[error("malformed sealed data")]
	Malformed,
	/// The passphrase is too weak to protect a life's worth of data.
	#[error("passphrase must be at least {0} characters")]
	WeakPassphrase(usize),
	/// The OS keychain refused.
	#[error("keychain: {0}")]
	Keychain(String),
	/// Key derivation failed.
	#[error("key derivation: {0}")]
	Kdf(String),
}

/// The vault's root key.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct VaultKey([u8; 32]);

impl VaultKey {
	/// A fresh random key.
	pub fn generate() -> Self {
		let mut key = [0u8; 32];
		getrandom::fill(&mut key).expect("the OS random source is available");
		Self(key)
	}

	/// Wrap existing key bytes.
	pub fn from_bytes(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}

	/// The raw key, for storing in a keychain.
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// A subkey for one purpose, such as `"sqlcipher"` or `"media"`.
	pub fn derive(&self, purpose: &str) -> SubKey {
		SubKey(blake3::derive_key(
			&format!("quant vault {purpose} v1"),
			&self.0,
		))
	}

	/// The `SQLCipher` raw key, as the `x'…'` literal `PRAGMA key` expects.
	pub fn sqlcipher_pragma(&self) -> zeroize::Zeroizing<String> {
		let sub = self.derive("sqlcipher");
		zeroize::Zeroizing::new(format!("x'{}'", hex::encode(sub.0)))
	}

	/// A stable, non-secret identifier for this key, to check that two copies
	/// match without revealing either.
	pub fn fingerprint(&self) -> String {
		let sub = self.derive("fingerprint");
		hex::encode(&sub.0[..8])
	}
}

impl std::fmt::Debug for VaultKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "VaultKey({})", self.fingerprint())
	}
}

/// A key derived from the vault key for one purpose.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SubKey([u8; 32]);

impl SubKey {
	/// The raw subkey.
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn subkeys_differ_by_purpose_and_are_stable() {
		let key = VaultKey::generate();
		assert_ne!(key.derive("sqlcipher").0, key.derive("media").0);
		assert_eq!(key.derive("media").0, key.derive("media").0);
		assert_ne!(key.derive("media").0, *key.as_bytes());
	}

	#[test]
	fn sqlcipher_pragma_is_a_blob_literal() {
		let pragma = VaultKey::generate().sqlcipher_pragma();
		assert!(pragma.starts_with("x'") && pragma.ends_with('\''));
		assert_eq!(pragma.len(), 67);
	}

	#[test]
	fn debug_never_prints_the_key() {
		let key = VaultKey::from_bytes([7; 32]);
		assert!(!format!("{key:?}").contains("7, 7"));
	}
}
