use chacha20poly1305::KeyInit;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::Payload;

use crate::CryptoError;
use crate::VaultKey;

const VERSION: u8 = 1;
const NONCE: usize = 24;

/// Seal `plain` under a raw 32-byte key: `version ‖ nonce ‖ ciphertext+tag`.
/// `aad` is authenticated but not stored; opening needs the same value.
pub(crate) fn seal_with(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> Vec<u8> {
	let cipher = XChaCha20Poly1305::new(key.into());
	let mut nonce = [0u8; NONCE];
	getrandom::fill(&mut nonce).expect("the OS random source is available");
	let ciphertext = cipher
		.encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
		.expect("XChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
	let mut out = Vec::with_capacity(1 + NONCE + ciphertext.len());
	out.push(VERSION);
	out.extend_from_slice(&nonce);
	out.extend_from_slice(&ciphertext);
	out
}

/// Open data made by [`seal_with`].
pub(crate) fn open_with(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
	if sealed.len() < 1 + NONCE + 16 || sealed[0] != VERSION {
		return Err(CryptoError::Malformed);
	}
	let (nonce, ciphertext) = sealed[1..].split_at(NONCE);
	XChaCha20Poly1305::new(key.into())
		.decrypt(
			XNonce::from_slice(nonce),
			Payload {
				msg: ciphertext,
				aad,
			},
		)
		.map_err(|_| CryptoError::Tampered)
}

/// Seals media (screenshots, audio) for the blob store. Each blob is bound to
/// its content hash, so one stored blob cannot be swapped for another.
#[derive(Clone)]
pub struct MediaSealer {
	key: crate::SubKey,
}

impl MediaSealer {
	/// A sealer using the vault's media subkey.
	pub fn new(vault: &VaultKey) -> Self {
		Self {
			key: vault.derive("media"),
		}
	}

	/// Seal media whose BLAKE3 hash is `hash`.
	pub fn seal(&self, hash: &str, plain: &[u8]) -> Vec<u8> {
		seal_with(self.key.as_bytes(), plain, hash.as_bytes())
	}

	/// Open media expected to hash to `hash`.
	pub fn open(&self, hash: &str, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
		let plain = open_with(self.key.as_bytes(), sealed, hash.as_bytes())?;
		if blake3::hash(&plain).to_hex().as_str() != hash {
			return Err(CryptoError::Tampered);
		}
		Ok(plain)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn media_round_trips_and_is_bound_to_its_hash() {
		let sealer = MediaSealer::new(&VaultKey::generate());
		let jpeg = b"not really a jpeg";
		let hash = blake3::hash(jpeg).to_hex().to_string();
		let sealed = sealer.seal(&hash, jpeg);
		assert_eq!(sealer.open(&hash, &sealed).unwrap(), jpeg);

		let other = blake3::hash(b"other").to_hex().to_string();
		assert!(matches!(
			sealer.open(&other, &sealed),
			Err(CryptoError::Tampered)
		));
	}

	#[test]
	fn detects_tampering_and_wrong_keys() {
		let key = VaultKey::generate();
		let sealer = MediaSealer::new(&key);
		let hash = blake3::hash(b"x").to_hex().to_string();
		let mut sealed = sealer.seal(&hash, b"x");
		let last = sealed.len() - 1;
		sealed[last] ^= 1;
		assert!(matches!(
			sealer.open(&hash, &sealed),
			Err(CryptoError::Tampered)
		));

		let sealed = sealer.seal(&hash, b"x");
		let stranger = MediaSealer::new(&VaultKey::generate());
		assert!(matches!(
			stranger.open(&hash, &sealed),
			Err(CryptoError::Tampered)
		));
		assert!(matches!(
			sealer.open(&hash, &[1, 2, 3]),
			Err(CryptoError::Malformed)
		));
	}
}
