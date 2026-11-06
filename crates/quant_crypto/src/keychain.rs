//! The device copy of the vault key, kept in the OS keychain.
//!
//! On macOS the item is created by the Quant binary, so other programs
//! (including an agent shelling out to `security`) trigger a system prompt
//! before they can read it.

use crate::CryptoError;
use crate::VaultKey;

const SERVICE: &str = "dev.leptoskit.quant";

fn entry(vault_id: &str) -> Result<keyring::Entry, CryptoError> {
	keyring::Entry::new(SERVICE, &format!("vault-key:{vault_id}"))
		.map_err(|e| CryptoError::Keychain(e.to_string()))
}

/// Store the vault key for `vault_id`.
pub fn store(vault_id: &str, key: &VaultKey) -> Result<(), CryptoError> {
	entry(vault_id)?
		.set_secret(key.as_bytes())
		.map_err(|e| CryptoError::Keychain(e.to_string()))
}

/// Load the vault key for `vault_id`, if the keychain has it.
pub fn load(vault_id: &str) -> Result<Option<VaultKey>, CryptoError> {
	match entry(vault_id)?.get_secret() {
		Ok(bytes) => {
			let mut bytes = zeroize::Zeroizing::new(bytes);
			let array: [u8; 32] = bytes
				.as_slice()
				.try_into()
				.map_err(|_| CryptoError::Malformed)?;
			bytes.iter_mut().for_each(|b| *b = 0);
			Ok(Some(VaultKey::from_bytes(array)))
		}
		Err(keyring::Error::NoEntry) => Ok(None),
		Err(e) => Err(CryptoError::Keychain(e.to_string())),
	}
}

/// Remove the vault key for `vault_id`.
pub fn forget(vault_id: &str) -> Result<(), CryptoError> {
	match entry(vault_id)?.delete_credential() {
		Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
		Err(e) => Err(CryptoError::Keychain(e.to_string())),
	}
}
