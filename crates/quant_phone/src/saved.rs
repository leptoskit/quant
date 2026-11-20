//! What the phone remembers between launches, in its private data
//! directory. Nothing here is secret: the spool key lives in the phone's
//! keystore and the signing key in its own owner-only file.

use std::path::Path;

use quant_core::DeviceId;
use serde::Deserialize;
use serde::Serialize;

/// The phone's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
	/// This phone's device id on everything it records.
	pub device: DeviceId,
	/// The pairing link the owner accepted, which names the vault's address
	/// and pins its certificate.
	#[serde(default)]
	pub link: Option<String>,
	/// Whether the owner wants this phone to collect.
	#[serde(default = "yes")]
	pub collecting: bool,
	/// Where the next app usage read starts, in Unix milliseconds.
	#[serde(default)]
	pub usage_until_ms: Option<i64>,
	/// Latest opaque incremental changes token from `HealthKit` / Health Connect.
	#[serde(default)]
	pub health_token: Option<String>,
	/// Where the next windowed health-store read starts, in Unix milliseconds.
	#[serde(default)]
	pub health_until_ms: Option<i64>,
}

fn yes() -> bool {
	true
}

impl Default for Saved {
	fn default() -> Self {
		Self {
			device: DeviceId::new(),
			link: None,
			collecting: true,
			usage_until_ms: None,
			health_token: None,
			health_until_ms: None,
		}
	}
}

impl Saved {
	/// Read the settings, or start new ones when there are none yet.
	pub fn load(path: &Path) -> std::io::Result<Self> {
		match std::fs::read(path) {
			Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				let saved = Self::default();
				// The device id must not change between launches.
				saved.save(path)?;
				Ok(saved)
			}
			Err(error) => Err(error),
		}
	}

	/// Write the settings, replacing the file in one step.
	pub fn save(&self, path: &Path) -> std::io::Result<()> {
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		let temp = path.with_extension("tmp");
		std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
		}
		std::fs::rename(temp, path)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_device_id_survives_a_restart() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("nested/phone.json");
		let first = Saved::load(&path).unwrap();
		assert!(first.collecting);
		assert_eq!(first.link, None);
		let mut changed = first.clone();
		changed.link = Some("quant://link?addr=10.0.0.2:7717&fp=ab".into());
		changed.collecting = false;
		changed.usage_until_ms = Some(42);
		changed.save(&path).unwrap();
		let again = Saved::load(&path).unwrap();
		assert_eq!(again, changed);
		assert_eq!(again.device, first.device);
	}

	#[test]
	fn older_files_keep_collecting_on() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("phone.json");
		let device = DeviceId::new();
		let json = serde_json::json!({ "device": device });
		std::fs::write(&path, json.to_string()).unwrap();
		let saved = Saved::load(&path).unwrap();
		assert_eq!(saved.device, device);
		assert!(saved.collecting);
	}

	#[test]
	fn a_damaged_file_is_an_error_not_a_new_device() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("phone.json");
		std::fs::write(&path, b"{").unwrap();
		assert!(Saved::load(&path).is_err());
	}
}
