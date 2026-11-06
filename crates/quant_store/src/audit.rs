use jiff::Timestamp;
use quant_access::audit::AuditEntry;
use quant_access::audit::AuditEvent;
use quant_access::audit::ChainBreak;
use quant_access::audit::GENESIS;
use quant_access::audit::verify_chain;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::Result;
use crate::Store;
use crate::from_micros;
use crate::micros;

impl Store {
	/// Append an event to the audit chain.
	pub fn audit(&self, event: AuditEvent) -> Result<AuditEntry> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let last: Option<(i64, String)> = tx
			.query_row(
				"SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1",
				[],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.optional()?;
		let (seq, prev) = last.map_or((1, GENESIS.to_owned()), |(seq, hash)| (seq + 1, hash));
		let entry = event.seal(u64::try_from(seq).unwrap_or(1), Timestamp::now(), &prev);
		tx.execute(
			"INSERT INTO audit (seq, at_us, actor, action, entry, hash) VALUES (?1, ?2, ?3, ?4, \
			 ?5, ?6)",
			params![
				seq,
				micros(entry.at),
				serde_json::to_string(&entry.actor)?,
				entry.action,
				serde_json::to_string(&entry)?,
				entry.hash,
			],
		)?;
		tx.commit()?;
		Ok(entry)
	}

	/// Audit entries at or after `since`, oldest first, at most `limit`.
	pub fn audit_log(&self, since: Timestamp, limit: usize) -> Result<Vec<AuditEntry>> {
		let conn = self.conn();
		let mut statement =
			conn.prepare_cached("SELECT entry FROM audit WHERE at_us >= ?1 ORDER BY seq LIMIT ?2")?;
		let rows = statement.query_map(
			params![micros(since), i64::try_from(limit).unwrap_or(i64::MAX)],
			|r| r.get::<_, String>(0),
		)?;
		rows.map(|entry| Ok(serde_json::from_str(&entry?)?))
			.collect()
	}

	/// The most recent `limit` audit entries, newest first.
	pub fn audit_tail(&self, limit: usize) -> Result<Vec<AuditEntry>> {
		let conn = self.conn();
		let mut statement =
			conn.prepare_cached("SELECT entry FROM audit ORDER BY seq DESC LIMIT ?1")?;
		let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
			r.get::<_, String>(0)
		})?;
		rows.map(|entry| Ok(serde_json::from_str(&entry?)?))
			.collect()
	}

	/// Verify the whole audit chain. Returns how many entries it holds.
	pub fn verify_audit(&self) -> Result<Result<u64, ChainBreak>> {
		let conn = self.conn();
		let mut statement = conn.prepare("SELECT entry FROM audit ORDER BY seq")?;
		let entries: Vec<AuditEntry> = statement
			.query_map([], |r| r.get::<_, String>(0))?
			.map(|entry| Ok(serde_json::from_str(&entry?)?))
			.collect::<Result<_>>()?;
		Ok(verify_chain(GENESIS, &entries).map(|()| entries.len() as u64))
	}

	/// When the first audit entry was written, if any.
	pub fn audit_started(&self) -> Result<Option<Timestamp>> {
		let first: Option<i64> =
			self.conn()
				.query_row("SELECT min(at_us) FROM audit", [], |r| r.get(0))?;
		first.map(from_micros).transpose()
	}
}

#[cfg(test)]
mod tests {
	use quant_access::audit::Actor;
	use quant_access::audit::Outcome;

	use super::*;
	use crate::test_support::store;

	#[test]
	fn appends_a_verifiable_chain_and_detects_tampering() {
		let store = store();
		for i in 0..5 {
			store
				.audit(AuditEvent::new(
					Actor::System,
					"query",
					"health.heart_rate",
					Outcome::Allowed {
						records: i,
						bytes: 0,
					},
				))
				.unwrap();
		}
		assert_eq!(store.verify_audit().unwrap(), Ok(5));
		assert_eq!(store.audit_tail(2).unwrap()[0].seq, 5);
		assert_eq!(store.audit_log(Timestamp::MIN, 100).unwrap().len(), 5);

		store
			.conn()
			.execute("DELETE FROM audit WHERE seq = 3", [])
			.unwrap();
		assert_eq!(store.verify_audit().unwrap(), Err(ChainBreak::Gap(4)));
	}
}
