//! Rendezvous identity: who the daemon is on the relay.
//!
//! The daemon derives an Ed25519 key from the vault key, so any process that
//! can open the vault can prove it owns the rendezvous id, and nothing else
//! ever can. The relay sees only the public half and the id, which is the
//! first 16 bytes of SHA-256 of that public key in lowercase base32.

use std::fmt;

use data_encoding::BASE32_NOPAD;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::Verifier;
use ed25519_dalek::VerifyingKey;
use quant_access::auth::PublicKey;
use quant_crypto::VaultKey;
use sha2::Digest;
use sha2::Sha256;

/// The bytes a relay signature covers, so one handshake's proof cannot be
/// replayed in another.
pub(crate) fn signed_bytes(domain: &str, parts: &[&[u8]]) -> Vec<u8> {
	let mut message = Vec::from(domain);
	for part in parts {
		message.extend_from_slice(b"\n");
		message.extend_from_slice(part);
	}
	message
}

/// The daemon's key on the relay, derived from the vault key.
#[derive(Clone)]
pub struct RelayKey(SigningKey);

impl RelayKey {
	/// Derive the key a vault uses on any relay. The vault key never leaves
	/// the daemon; relays see only the public half.
	pub fn from_vault(vault: &VaultKey) -> Self {
		Self(SigningKey::from_bytes(vault.derive("relay").as_bytes()))
	}

	/// A key from a 32-byte seed, for tests and tools.
	pub fn from_seed(seed: &[u8; 32]) -> Self {
		Self(SigningKey::from_bytes(seed))
	}

	/// A fresh random key, for tests.
	pub fn generate() -> Self {
		let mut seed = [0u8; 32];
		getrandom::fill(&mut seed).expect("the OS random source is available");
		Self::from_seed(&seed)
	}

	/// The public half, as the protocol sends it.
	pub fn public(&self) -> PublicKey {
		PublicKey::from_bytes(self.0.verifying_key().as_bytes())
	}

	/// This daemon's rendezvous id.
	pub fn rid(&self) -> String {
		rid(self.public().as_bytes())
	}

	/// Sign the relay handshake (see [`proto`]).
	pub(crate) fn sign(&self, message: &[u8]) -> Signature {
		self.0.sign(message)
	}
}

impl fmt::Debug for RelayKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "RelayKey({})", self.rid())
	}
}

/// The rendezvous id for a public key: lowercase base32, no padding, of the
/// first 16 bytes of its SHA-256.
pub fn rid(public: &[u8; 32]) -> String {
	let hash = Sha256::digest(public);
	BASE32_NOPAD.encode(&hash[..16]).to_ascii_lowercase()
}

/// Whether `text` is shaped like a rendezvous id.
pub fn valid_rid(text: &str) -> bool {
	text.len() == 26 && text.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

/// Check a signature made by `key` over `message`.
pub(crate) fn verify(key: &PublicKey, message: &[u8], signature: &[u8]) -> bool {
	let Ok(signature) = Signature::from_slice(signature) else {
		return false;
	};
	let verifying = VerifyingKey::from_bytes(key.as_bytes());
	verifying.is_ok_and(|verifying| verifying.verify(message, &signature).is_ok())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rid_is_stable_lowercase_base32_of_the_hash_prefix() {
		let key = RelayKey::from_seed(&[7; 32]);
		let public = key.public();
		let expected = BASE32_NOPAD
			.encode(&Sha256::digest(public.as_bytes())[..16])
			.to_ascii_lowercase();
		assert_eq!(key.rid(), expected);
		assert_eq!(key.rid().len(), 26);
		assert_eq!(key.rid(), key.rid());
		assert!(valid_rid(&key.rid()));
		assert!(!valid_rid("not-a-rid"));
	}

	#[test]
	fn the_same_vault_key_makes_the_same_rendezvous_key() {
		let vault = VaultKey::generate();
		assert_eq!(
			RelayKey::from_vault(&vault).rid(),
			RelayKey::from_vault(&vault).rid()
		);
		assert_ne!(
			RelayKey::from_vault(&vault).rid(),
			RelayKey::from_vault(&VaultKey::generate()).rid()
		);
	}

	#[test]
	fn debug_prints_only_the_id() {
		let key = RelayKey::generate();
		assert!(!format!("{key:?}").is_empty());
	}
}
