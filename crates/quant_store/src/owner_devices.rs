use jiff::Timestamp;
use quant_access::OwnerDevicePolicy;
use quant_core::OwnerDeviceId;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::Result;
use crate::Store;
use crate::StoreError;
use crate::from_micros;
use crate::micros;

/// Most owner devices that may be active at once, so the owner's list stays
/// something they actually read.
pub const MAX_OWNER_DEVICES: usize = 8;

/// A hardware key the owner enrolled to open owner sessions. Only the public
/// half is stored: the private key never leaves the device's Secure Enclave
/// or Keystore.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerDevice {
	/// Its id.
	pub id: OwnerDeviceId,
	/// The name the owner gave it.
	pub label: String,
	/// The device's P-256 public key, uncompressed SEC1 (65 bytes).
	pub public_key: Vec<u8>,
	/// How the device protects its key, as it reported at enrolment.
	pub policy: OwnerDevicePolicy,
	/// When it was enrolled.
	pub created_at: Timestamp,
	/// When it last opened a session.
	pub last_used: Option<Timestamp>,
	/// When the owner revoked it.
	pub revoked_at: Option<Timestamp>,
}

fn policy_from_text(text: &str) -> Result<OwnerDevicePolicy> {
	OwnerDevicePolicy::parse(text).ok_or_else(|| StoreError::Corrupt("owner device policy"))
}

fn device_from_row(row: &Row<'_>) -> rusqlite::Result<Result<OwnerDevice>> {
	let id: Vec<u8> = row.get(0)?;
	let label: String = row.get(1)?;
	let public_key: Vec<u8> = row.get(2)?;
	let policy: String = row.get(3)?;
	let created: i64 = row.get(4)?;
	let last_used: Option<i64> = row.get(5)?;
	let revoked: Option<i64> = row.get(6)?;
	Ok((|| {
		Ok(OwnerDevice {
			id: OwnerDeviceId(
				Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("owner device id"))?,
			),
			label,
			public_key,
			policy: policy_from_text(&policy)?,
			created_at: from_micros(created)?,
			last_used: last_used.map(from_micros).transpose()?,
			revoked_at: revoked.map(from_micros).transpose()?,
		})
	})())
}

const DEVICE_COLUMNS: &str = "id, label, public_key, policy, created_us, last_used_us, revoked_us";

impl Store {
	/// Every owner device, newest first.
	pub fn owner_devices(&self) -> Result<Vec<OwnerDevice>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&format!(
			"SELECT {DEVICE_COLUMNS} FROM owner_devices ORDER BY created_us DESC"
		))?;
		let rows = statement.query_map([], device_from_row)?;
		rows.map(|row| row?).collect()
	}

	/// An owner device by id.
	pub fn owner_device(&self, id: OwnerDeviceId) -> Result<Option<OwnerDevice>> {
		self.conn()
			.query_row(
				&format!("SELECT {DEVICE_COLUMNS} FROM owner_devices WHERE id = ?1"),
				[id.0.as_bytes().as_slice()],
				device_from_row,
			)
			.optional()?
			.transpose()
	}

	/// Enrol a device. The key must be a valid uncompressed SEC1 P-256 point;
	/// the daemon checks before storing.
	pub fn add_owner_device(
		&self,
		label: &str,
		public_key: &[u8],
		policy: OwnerDevicePolicy,
	) -> Result<OwnerDevice> {
		let active: i64 = self.conn().query_row(
			"SELECT count(*) FROM owner_devices WHERE revoked_us IS NULL",
			[],
			|r| r.get(0),
		)?;
		if usize::try_from(active).unwrap_or(usize::MAX) >= MAX_OWNER_DEVICES {
			return Err(StoreError::Invalid(format!(
				"at most {MAX_OWNER_DEVICES} owner devices may be active; revoke one first"
			)));
		}
		let device = OwnerDevice {
			id: OwnerDeviceId::new(),
			label: label.to_owned(),
			public_key: public_key.to_vec(),
			policy,
			created_at: crate::now_micros()?,
			last_used: None,
			revoked_at: None,
		};
		self.conn().execute(
			&format!(
				"INSERT INTO owner_devices ({DEVICE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, NULL, \
				 NULL)"
			),
			params![
				device.id.0.as_bytes().as_slice(),
				device.label,
				device.public_key,
				device.policy.as_str(),
				micros(device.created_at),
			],
		)?;
		Ok(device)
	}

	/// Revoke a device, if it is not revoked already. Its key stops working
	/// at once.
	pub fn revoke_owner_device(&self, id: OwnerDeviceId) -> Result<OwnerDevice> {
		// The guard is dropped before the re-read below locks the connection
		// again.
		let changed = {
			let conn = self.conn();
			conn.execute(
				"UPDATE owner_devices SET revoked_us = ?2 WHERE id = ?1 AND revoked_us IS NULL",
				params![id.0.as_bytes().as_slice(), micros(Timestamp::now())],
			)?
		};
		if changed == 0 && self.owner_device(id)?.is_none() {
			return Err(StoreError::NotFound);
		}
		self.owner_device(id)?.ok_or(StoreError::NotFound)
	}

	/// Note that a device just opened a session.
	pub fn touch_owner_device(&self, id: OwnerDeviceId) -> Result<()> {
		self.conn().execute(
			"UPDATE owner_devices SET last_used_us = ?2 WHERE id = ?1",
			params![id.0.as_bytes().as_slice(), micros(Timestamp::now())],
		)?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::store;

	/// A valid uncompressed SEC1 P-256 point (the generator), for storage
	/// tests; the daemon validates keys properly before they reach here.
	fn key(byte: u8) -> Vec<u8> {
		let mut key = vec![0x04];
		key.extend(std::iter::repeat_n(byte, 64));
		key
	}

	#[test]
	fn devices_are_enrolled_listed_and_revoked() {
		let store = store();
		let a = store
			.add_owner_device("MacBook", &key(1), OwnerDevicePolicy::Biometry)
			.unwrap();
		let b = store
			.add_owner_device("Pixel", &key(2), OwnerDevicePolicy::BiometryOrPasscode)
			.unwrap();
		assert_eq!(store.owner_devices().unwrap().len(), 2);
		assert_eq!(store.owner_device(a.id).unwrap().unwrap().label, "MacBook");
		assert_eq!(
			store.owner_device(b.id).unwrap().unwrap().policy,
			OwnerDevicePolicy::BiometryOrPasscode
		);

		let revoked = store.revoke_owner_device(a.id).unwrap();
		assert!(revoked.revoked_at.is_some());
		// Revoking again changes nothing.
		assert_eq!(store.revoke_owner_device(a.id).unwrap(), revoked);
		assert_eq!(
			store
				.owner_devices()
				.unwrap()
				.iter()
				.filter(|d| d.revoked_at.is_none())
				.count(),
			1
		);
		assert_eq!(store.owner_device(OwnerDeviceId::new()).unwrap(), None);
		assert!(matches!(
			store.revoke_owner_device(OwnerDeviceId::new()),
			Err(StoreError::NotFound)
		));
	}

	#[test]
	fn enrolling_caps_at_eight_active_devices() {
		let store = store();
		for index in 0..MAX_OWNER_DEVICES {
			store
				.add_owner_device(
					"key",
					&key(u8::try_from(index).unwrap() + 1),
					OwnerDevicePolicy::Biometry,
				)
				.unwrap();
		}
		assert!(matches!(
			store.add_owner_device("one more", &key(9), OwnerDevicePolicy::Biometry),
			Err(StoreError::Invalid(_))
		));
		// Revoked devices leave room for new ones.
		let first = store.owner_devices().unwrap().pop().unwrap();
		store.revoke_owner_device(first.id).unwrap();
		assert!(
			store
				.add_owner_device("one more", &key(10), OwnerDevicePolicy::Biometry)
				.is_ok()
		);
	}
}
