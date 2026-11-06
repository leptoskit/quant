use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use quant_access::auth::ClientKey;
use quant_core::ClientId;
use serde::Deserialize;
use serde::Serialize;

/// A client's name, id, and signing key.
#[derive(Clone, Debug)]
pub struct Identity {
	/// A name the owner will recognise.
	pub name: String,
	/// Assigned when pairing.
	pub client: Option<ClientId>,
	key: Arc<ClientKey>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
	name: String,
	client: Option<ClientId>,
	/// Hex seed of the signing key.
	key: String,
}

impl Identity {
	/// A new identity with a fresh key.
	pub fn generate(name: impl Into<String>) -> Self {
		Self {
			name: name.into(),
			client: None,
			key: Arc::new(ClientKey::generate()),
		}
	}

	/// The signing key.
	pub fn key(&self) -> &ClientKey {
		&self.key
	}

	/// Where identities live: `$QUANT_CONFIG`, or the platform config dir.
	pub fn default_dir() -> PathBuf {
		if let Some(dir) = std::env::var_os("QUANT_CONFIG") {
			return PathBuf::from(dir);
		}
		let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
		if cfg!(target_os = "macos") {
			home.join("Library/Application Support/Quant/clients")
		} else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
			PathBuf::from(config).join("quant")
		} else {
			home.join(".config/quant")
		}
	}

	/// The file for a named profile.
	pub fn path(dir: &Path, profile: &str) -> PathBuf {
		dir.join(format!("{profile}.identity.json"))
	}

	/// Load an identity.
	pub fn load(path: &Path) -> std::io::Result<Self> {
		let stored: Stored = serde_json::from_slice(&fs::read(path)?)?;
		let seed =
			hex_seed(&stored.key).ok_or_else(|| std::io::Error::other("corrupt identity key"))?;
		Ok(Self {
			name: stored.name,
			client: stored.client,
			key: Arc::new(ClientKey::from_seed(&seed)),
		})
	}

	/// Save an identity, readable by the owner only.
	pub fn save(&self, path: &Path) -> std::io::Result<()> {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}
		let seed = self.key.seed();
		let stored = Stored {
			name: self.name.clone(),
			client: self.client,
			key: hex::encode(seed),
		};
		let temp = path.with_extension("tmp");
		fs::write(&temp, serde_json::to_vec_pretty(&stored)?)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
		}
		fs::rename(temp, path)
	}
}

fn hex_seed(hex: &str) -> Option<[u8; 32]> {
	if hex.len() != 64 {
		return None;
	}
	let mut seed = [0u8; 32];
	for (i, byte) in seed.iter_mut().enumerate() {
		*byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
	}
	Some(seed)
}
