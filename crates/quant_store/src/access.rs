use jiff::Timestamp;
use quant_access::Grant;
use quant_access::Scopes;
use quant_access::auth::PublicKey;
use quant_core::ClientId;
use quant_core::GrantId;
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

/// What sort of client this is, shown to the owner when approving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
	/// An AI agent, usually through the MCP server.
	Agent,
	/// The `quant` command line.
	Cli,
	/// A capturing device: a phone, a watch bridge, custom hardware.
	Device,
	/// The Quant app.
	App,
}

impl ClientKind {
	fn as_str(self) -> &'static str {
		match self {
			ClientKind::Agent => "agent",
			ClientKind::Cli => "cli",
			ClientKind::Device => "device",
			ClientKind::App => "app",
		}
	}

	fn parse(s: &str) -> Result<Self> {
		match s {
			"agent" => Ok(ClientKind::Agent),
			"cli" => Ok(ClientKind::Cli),
			"device" => Ok(ClientKind::Device),
			"app" => Ok(ClientKind::App),
			_ => Err(StoreError::Corrupt("client kind")),
		}
	}
}

/// Where a client stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientStatus {
	/// Asked to pair; waiting for the owner.
	Pending,
	/// Approved, with a grant.
	Active,
	/// Refused or revoked. Its key can never pair again.
	Revoked,
}

impl ClientStatus {
	fn as_str(self) -> &'static str {
		match self {
			ClientStatus::Pending => "pending",
			ClientStatus::Active => "active",
			ClientStatus::Revoked => "revoked",
		}
	}

	fn parse(s: &str) -> Result<Self> {
		match s {
			"pending" => Ok(ClientStatus::Pending),
			"active" => Ok(ClientStatus::Active),
			"revoked" => Ok(ClientStatus::Revoked),
			_ => Err(StoreError::Corrupt("client status")),
		}
	}
}

/// A client known to the vault.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Client {
	/// Its id.
	pub id: ClientId,
	/// The name it gave, such as "Claude Code".
	pub name: String,
	/// What sort of client.
	pub kind: ClientKind,
	/// Its public signing key.
	pub public_key: PublicKey,
	/// Where it stands.
	pub status: ClientStatus,
	/// Scopes it asked for when pairing.
	pub requested: Scopes,
	/// When it asked.
	pub created_at: Timestamp,
	/// When it last made a request.
	pub last_seen: Option<Timestamp>,
}

impl Client {
	/// The code the owner compares when approving.
	pub fn pairing_code(&self) -> String {
		self.public_key.pairing_code()
	}
}

fn client_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Client>> {
	let id: Vec<u8> = row.get(0)?;
	let name: String = row.get(1)?;
	let kind: String = row.get(2)?;
	let key: String = row.get(3)?;
	let status: String = row.get(4)?;
	let requested: String = row.get(5)?;
	let created: i64 = row.get(6)?;
	let seen: Option<i64> = row.get(7)?;
	Ok((|| {
		Ok(Client {
			id: ClientId(Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("client id"))?),
			name,
			kind: ClientKind::parse(&kind)?,
			public_key: key.parse().map_err(|_| StoreError::Corrupt("client key"))?,
			status: ClientStatus::parse(&status)?,
			requested: serde_json::from_str(&requested)?,
			created_at: from_micros(created)?,
			last_seen: seen.map(from_micros).transpose()?,
		})
	})())
}

const CLIENT_COLUMNS: &str =
	"id, name, kind, public_key, status, requested, created_us, last_seen_us";

/// Most clients that may wait for approval at once, so an unauthenticated
/// caller cannot flood the owner's list.
pub const MAX_PENDING: usize = 16;

impl Store {
	/// Record a pairing request. Asking again with the same key returns the
	/// existing client unchanged.
	pub fn register_client(
		&self,
		name: &str,
		kind: ClientKind,
		public_key: PublicKey,
		requested: &Scopes,
	) -> Result<Client> {
		if let Some(existing) = self.client_by_key(&public_key)? {
			return Ok(existing);
		}
		let pending: i64 = self.conn().query_row(
			"SELECT count(*) FROM clients WHERE status = 'pending'",
			[],
			|r| r.get(0),
		)?;
		if usize::try_from(pending).unwrap_or(usize::MAX) >= MAX_PENDING {
			return Err(StoreError::Invalid(
				"too many pairing requests are waiting".into(),
			));
		}
		let name: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
		let client = Client {
			id: ClientId::new(),
			name,
			kind,
			public_key,
			status: ClientStatus::Pending,
			requested: requested.clone(),
			created_at: Timestamp::now(),
			last_seen: None,
		};
		self.conn().execute(
			&format!(
				"INSERT INTO clients ({CLIENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)"
			),
			params![
				client.id.0.as_bytes().as_slice(),
				client.name,
				client.kind.as_str(),
				client.public_key.to_string(),
				client.status.as_str(),
				serde_json::to_string(&client.requested)?,
				micros(client.created_at),
			],
		)?;
		Ok(client)
	}

	/// A client by id.
	pub fn client(&self, id: ClientId) -> Result<Option<Client>> {
		self.conn()
			.query_row(
				&format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE id = ?1"),
				[id.0.as_bytes().as_slice()],
				client_from_row,
			)
			.optional()?
			.transpose()
	}

	/// A client by public key.
	pub fn client_by_key(&self, key: &PublicKey) -> Result<Option<Client>> {
		self.conn()
			.query_row(
				&format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE public_key = ?1"),
				[key.to_string()],
				client_from_row,
			)
			.optional()?
			.transpose()
	}

	/// Every client, newest first.
	pub fn clients(&self) -> Result<Vec<Client>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&format!(
			"SELECT {CLIENT_COLUMNS} FROM clients ORDER BY created_us DESC"
		))?;
		let rows = statement.query_map([], client_from_row)?;
		rows.map(|row| row?).collect()
	}

	/// Find a pending client by the start of its id or by its pairing code.
	pub fn find_pending(&self, needle: &str) -> Result<Option<Client>> {
		let needle = needle.trim().to_ascii_uppercase();
		Ok(self.clients()?.into_iter().find(|client| {
			client.status == ClientStatus::Pending
				&& (client.pairing_code() == needle
					|| client.pairing_code().replace('-', "") == needle.replace('-', "")
					|| client
						.id
						.to_string()
						.to_ascii_uppercase()
						.starts_with(&needle))
		}))
	}

	/// Change a client's status.
	pub fn set_client_status(&self, id: ClientId, status: ClientStatus) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE clients SET status = ?2 WHERE id = ?1",
			params![id.0.as_bytes().as_slice(), status.as_str()],
		)?;
		if changed == 0 {
			Err(StoreError::NotFound)
		} else {
			Ok(())
		}
	}

	/// Note that a client just made a request.
	pub fn touch_client(&self, id: ClientId) -> Result<()> {
		self.conn().execute(
			"UPDATE clients SET last_seen_us = ?2 WHERE id = ?1",
			params![id.0.as_bytes().as_slice(), micros(Timestamp::now())],
		)?;
		Ok(())
	}

	/// Activate a client with `grant`, revoking any grant it held before.
	pub fn approve(&self, grant: &Grant) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let client = grant.client.0.as_bytes().as_slice();
		let revoked_at = serde_json::to_value(Timestamp::now())?;
		let previous: Vec<(Vec<u8>, String)> = {
			let mut statement =
				tx.prepare("SELECT id, body FROM grants WHERE client = ?1 AND revoked = 0")?;
			let rows = statement.query_map([client], |r| Ok((r.get(0)?, r.get(1)?)))?;
			rows.collect::<rusqlite::Result<_>>()?
		};
		for (id, body) in previous {
			let mut old: serde_json::Value = serde_json::from_str(&body)?;
			old["revoked_at"] = revoked_at.clone();
			tx.execute(
				"UPDATE grants SET revoked = 1, body = ?2 WHERE id = ?1",
				params![id, serde_json::to_string(&old)?],
			)?;
		}
		tx.execute(
			"INSERT INTO grants (id, client, body, revoked) VALUES (?1, ?2, ?3, 0)",
			params![
				grant.id.0.as_bytes().as_slice(),
				client,
				serde_json::to_string(grant)?
			],
		)?;
		let changed = tx.execute(
			"UPDATE clients SET status = 'active' WHERE id = ?1",
			[client],
		)?;
		if changed == 0 {
			return Err(StoreError::NotFound);
		}
		tx.commit()?;
		Ok(())
	}

	/// The live grant a client holds, if any.
	pub fn active_grant(&self, client: ClientId) -> Result<Option<Grant>> {
		let body: Option<String> = self
			.conn()
			.query_row(
				"SELECT body FROM grants WHERE client = ?1 AND revoked = 0 ORDER BY rowid DESC \
				 LIMIT 1",
				[client.0.as_bytes().as_slice()],
				|r| r.get(0),
			)
			.optional()?;
		body.map(|b| serde_json::from_str(&b).map_err(Into::into))
			.transpose()
	}

	/// Every grant, live and revoked, newest first.
	pub fn grants(&self) -> Result<Vec<Grant>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached("SELECT body FROM grants ORDER BY rowid DESC")?;
		let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
		rows.map(|body| Ok(serde_json::from_str(&body?)?)).collect()
	}

	/// Replace a grant's terms (its scopes, limits, lookback, or expiry).
	pub fn update_grant(&self, grant: &Grant) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE grants SET body = ?2 WHERE id = ?1 AND revoked = 0",
			params![
				grant.id.0.as_bytes().as_slice(),
				serde_json::to_string(grant)?
			],
		)?;
		if changed == 0 {
			Err(StoreError::NotFound)
		} else {
			Ok(())
		}
	}

	/// Revoke a grant and the client holding it.
	pub fn revoke_grant(&self, id: GrantId) -> Result<Grant> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let body: String = tx
			.query_row(
				"SELECT body FROM grants WHERE id = ?1",
				[id.0.as_bytes().as_slice()],
				|r| r.get(0),
			)
			.optional()?
			.ok_or(StoreError::NotFound)?;
		let mut grant: Grant = serde_json::from_str(&body)?;
		if grant.revoked_at.is_none() {
			grant.revoked_at = Some(Timestamp::now());
			grant.epoch += 1;
		}
		tx.execute(
			"UPDATE grants SET revoked = 1, body = ?2 WHERE id = ?1",
			params![id.0.as_bytes().as_slice(), serde_json::to_string(&grant)?],
		)?;
		tx.execute(
			"UPDATE clients SET status = 'revoked' WHERE id = ?1",
			[grant.client.0.as_bytes().as_slice()],
		)?;
		tx.commit()?;
		Ok(grant)
	}
}

#[cfg(test)]
mod tests {
	use quant_access::auth::ClientKey;

	use super::*;
	use crate::test_support::store;

	#[test]
	fn pairing_is_idempotent_per_key() {
		let store = store();
		let key = ClientKey::generate().public();
		let scopes = Scopes::parse(["health:summary"]).unwrap();
		let a = store
			.register_client("Claude", ClientKind::Agent, key, &scopes)
			.unwrap();
		let b = store
			.register_client("Other name", ClientKind::Agent, key, &scopes)
			.unwrap();
		assert_eq!(a, b);
		assert_eq!(a.status, ClientStatus::Pending);
		assert_eq!(
			store.find_pending(&a.pairing_code()).unwrap(),
			Some(a.clone())
		);
		assert_eq!(
			store
				.find_pending(&a.pairing_code().replace('-', "").to_lowercase())
				.unwrap(),
			Some(a)
		);
	}

	#[test]
	fn caps_waiting_pairings() {
		let store = store();
		for _ in 0..MAX_PENDING {
			store
				.register_client(
					"x",
					ClientKind::Agent,
					ClientKey::generate().public(),
					&Scopes::default(),
				)
				.unwrap();
		}
		let overflow = store.register_client(
			"x",
			ClientKind::Agent,
			ClientKey::generate().public(),
			&Scopes::default(),
		);
		assert!(matches!(overflow, Err(StoreError::Invalid(_))));
	}

	#[test]
	fn approving_replaces_the_previous_grant_and_revoking_disables_the_client() {
		let store = store();
		let client = store
			.register_client(
				"Claude",
				ClientKind::Agent,
				ClientKey::generate().public(),
				&Scopes::default(),
			)
			.unwrap();
		let first = Grant::new(
			client.id,
			"first",
			Scopes::parse(["health:summary"]).unwrap(),
		);
		store.approve(&first).unwrap();
		assert_eq!(
			store.client(client.id).unwrap().unwrap().status,
			ClientStatus::Active
		);

		let second = Grant::new(client.id, "second", Scopes::parse(["*:read"]).unwrap());
		store.approve(&second).unwrap();
		assert_eq!(
			store.active_grant(client.id).unwrap().unwrap().id,
			second.id
		);
		let all = store.grants().unwrap();
		assert_eq!(all.len(), 2);
		assert!(
			all.iter()
				.find(|g| g.id == first.id)
				.unwrap()
				.revoked_at
				.is_some()
		);

		let revoked = store.revoke_grant(second.id).unwrap();
		assert!(revoked.revoked_at.is_some());
		assert_eq!(revoked.epoch, second.epoch + 1);
		assert_eq!(store.active_grant(client.id).unwrap(), None);
		assert_eq!(
			store.client(client.id).unwrap().unwrap().status,
			ClientStatus::Revoked
		);
	}
}
