use argon2::Algorithm;
use argon2::Argon2;
use argon2::Params;
use argon2::Version;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::CryptoError;
use crate::VaultKey;
use crate::seal;

/// Shortest passphrase accepted for a new vault.
pub const MIN_PASSPHRASE: usize = 12;

/// Argon2id cost settings, stored with the wrapped key so they can rise over
/// time without breaking old vaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
	/// Memory in KiB.
	pub memory_kib: u32,
	/// Passes.
	pub iterations: u32,
	/// Lanes.
	pub parallelism: u32,
}

impl KdfParams {
	/// The default: 64 MiB, 3 passes. About a third of a second on a laptop,
	/// which also paces anyone guessing.
	pub const DEFAULT: KdfParams = KdfParams {
		memory_kib: 64 * 1024,
		iterations: 3,
		parallelism: 1,
	};
	/// Cheap settings for tests only.
	pub const INSECURE_FAST: KdfParams = KdfParams {
		memory_kib: 64,
		iterations: 1,
		parallelism: 1,
	};
}

/// The vault key sealed under a passphrase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
	/// Always `argon2id+xchacha20poly1305`.
	pub alg: String,
	/// Argon2id settings.
	pub kdf: KdfParams,
	/// Base64 salt.
	pub salt: String,
	/// Base64 sealed key.
	pub sealed: String,
}

const ALG: &str = "argon2id+xchacha20poly1305";

fn derive(
	passphrase: &str,
	salt: &[u8],
	params: KdfParams,
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
	let params = Params::new(
		params.memory_kib,
		params.iterations,
		params.parallelism,
		Some(32),
	)
	.map_err(|e| CryptoError::Kdf(e.to_string()))?;
	let mut out = Zeroizing::new([0u8; 32]);
	Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
		.hash_password_into(passphrase.as_bytes(), salt, out.as_mut())
		.map_err(|e| CryptoError::Kdf(e.to_string()))?;
	Ok(out)
}

impl WrappedKey {
	/// Seal `key` under `passphrase`.
	pub fn wrap(key: &VaultKey, passphrase: &str, params: KdfParams) -> Result<Self, CryptoError> {
		if passphrase.chars().count() < MIN_PASSPHRASE {
			return Err(CryptoError::WeakPassphrase(MIN_PASSPHRASE));
		}
		let mut salt = [0u8; 16];
		getrandom::fill(&mut salt).expect("the OS random source is available");
		let kek = derive(passphrase, &salt, params)?;
		let sealed = seal::seal_with(&kek, key.as_bytes(), ALG.as_bytes());
		Ok(Self {
			alg: ALG.to_owned(),
			kdf: params,
			salt: STANDARD.encode(salt),
			sealed: STANDARD.encode(sealed),
		})
	}

	/// Recover the key with `passphrase`.
	pub fn unwrap(&self, passphrase: &str) -> Result<VaultKey, CryptoError> {
		if self.alg != ALG {
			return Err(CryptoError::Malformed);
		}
		let salt = STANDARD
			.decode(&self.salt)
			.map_err(|_| CryptoError::Malformed)?;
		let sealed = STANDARD
			.decode(&self.sealed)
			.map_err(|_| CryptoError::Malformed)?;
		let kek = derive(passphrase, &salt, self.kdf)?;
		let plain = seal::open_with(&kek, &sealed, ALG.as_bytes()).map_err(|error| {
			match error {
				CryptoError::Tampered => CryptoError::WrongPassphrase,
				other => other,
			}
		})?;
		let bytes: [u8; 32] = plain
			.as_slice()
			.try_into()
			.map_err(|_| CryptoError::Malformed)?;
		Ok(VaultKey::from_bytes(bytes))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wraps_and_unwraps() {
		let key = VaultKey::generate();
		let wrapped =
			WrappedKey::wrap(&key, "correct horse battery", KdfParams::INSECURE_FAST).unwrap();
		assert_eq!(wrapped.unwrap("correct horse battery").unwrap(), key);
		assert!(matches!(
			wrapped.unwrap("wrong horse battery"),
			Err(CryptoError::WrongPassphrase)
		));
	}

	#[test]
	fn refuses_short_passphrases() {
		let key = VaultKey::generate();
		assert!(matches!(
			WrappedKey::wrap(&key, "short", KdfParams::INSECURE_FAST),
			Err(CryptoError::WeakPassphrase(_))
		));
	}

	#[test]
	fn same_key_wraps_differently_each_time() {
		let key = VaultKey::generate();
		let a = WrappedKey::wrap(&key, "correct horse battery", KdfParams::INSECURE_FAST).unwrap();
		let b = WrappedKey::wrap(&key, "correct horse battery", KdfParams::INSECURE_FAST).unwrap();
		assert_ne!(a.sealed, b.sealed);
	}
}
