//! The relay's storage behind the shared protocol: registrations, invites,
//! and usage counters in `leptoskit_db`.

use leptoskit_db::Db;
use quant_relay::store::RelayStore;
use quant_relay::store::StoreError;

use crate::Invite;
use crate::Registration;
use crate::Usage;

/// A database the relay can serve from.
#[derive(Clone)]
pub struct RelayDb(Db);

/// A database error, as the relay's protocol reports it. `map_err` hands
/// over ownership, which is what this takes.
#[expect(
	clippy::needless_pass_by_value,
	reason = "map_err hands over ownership"
)]
fn failed(error: leptoskit_db::DbError) -> StoreError {
	StoreError::Failed(error.to_string())
}

impl RelayDb {
	/// A relay over `db`, which the server has already migrated.
	pub fn new(db: Db) -> Self {
		Self(db)
	}

	/// The database underneath, for migrations and the invite commands.
	pub fn db(&self) -> &Db {
		&self.0
	}
}

impl RelayStore for RelayDb {
	async fn register(
		&self,
		rid: &str,
		public_key: &str,
		invite: Option<&str>,
	) -> Result<(), StoreError> {
		let now = leptoskit_db::now();
		let rid = rid.to_owned();
		let public_key = public_key.to_owned();
		let invite = invite.map(ToOwned::to_owned);
		// A manual transaction, so the semantic errors (invite spent, key
		// mismatch) roll back without being squeezed into the database's own
		// error type.
		let tx = self.0.begin().await.map_err(failed)?;
		let result: Result<(), StoreError> = async {
			let existing = Registration::query()
				.filter(|row| row.rid.eq(rid.clone()))
				.first(&tx)
				.await
				.map_err(failed)?;
			if let Some(existing) = existing {
				return (existing.public_key == public_key)
					.then_some(())
					.ok_or(StoreError::KeyMismatch);
			}
			// A relay with any invite at all gates registration: the
			// operator who creates one means to choose who joins.
			let invite_id = if Invite::query().count(&tx).await.map_err(failed)? > 0 {
				let code = invite.ok_or(StoreError::InviteRequired)?;
				let mut found = Invite::query()
					.filter(|row| row.code.eq(code))
					.first(&tx)
					.await
					.map_err(failed)?
					.ok_or(StoreError::InviteUsed)?;
				if found.used_by.is_some() {
					return Err(StoreError::InviteUsed);
				}
				found.used_by = Some(rid.clone());
				found.used_at = Some(now);
				found.save(&tx).await.map_err(failed)?;
				Some(found.id)
			} else {
				None
			};
			crate::NewRegistration {
				rid,
				public_key,
				last_seen: now,
				invite_id,
			}
			.insert(&tx)
			.await
			.map_err(failed)?;
			Ok(())
		}
		.await;
		match result {
			Ok(()) => tx.commit().await.map_err(failed),
			Err(error) => {
				let _ = tx.rollback().await;
				Err(error)
			}
		}
	}

	async fn touch(&self, rid: &str) {
		let _ = Registration::update()
			.filter(|row| row.rid.eq(rid.to_owned()))
			.set(|row| row.last_seen, leptoskit_db::now())
			.run(&self.0)
			.await;
	}

	async fn add_usage(&self, rid: &str, day: &str, bytes: u64) {
		let key = format!("{rid}/{day}");
		let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
		let result = self
			.0
			.transaction(async |tx| {
				let result: Result<(), leptoskit_db::DbError> = async {
					if let Some(mut row) = Usage::query()
						.filter(|row| row.usage_key.eq(key.clone()))
						.first(tx)
						.await?
					{
						row.bytes = row.bytes.saturating_add(bytes);
						row.save(tx).await?;
					} else {
						crate::NewUsage {
							usage_key: key,
							rid: rid.to_owned(),
							day: day.to_owned(),
							bytes,
						}
						.insert(tx)
						.await?;
					}
					Ok(())
				}
				.await;
				result
			})
			.await;
		if let Err(error) = result {
			eprintln!("relay: could not record usage: {error}");
		}
	}

	async fn usage(&self, rid: &str, day: &str) -> Result<u64, StoreError> {
		let row = Usage::query()
			.filter(|row| row.rid.eq(rid.to_owned()) & row.day.eq(day.to_owned()))
			.first(&self.0)
			.await
			.map_err(failed)?;
		Ok(row.map_or(0, |row| u64::try_from(row.bytes).unwrap_or(0)))
	}
}
