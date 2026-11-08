//! Claims about the owner, stored beside the records that support them.
//!
//! A claim's statement is indexed by FTS5 the way records are; its evidence
//! ids live in [`claim_evidence`](schema), so purging a record removes it
//! from every claim that cited it.

use jiff::Timestamp;
use quant_core::Authority;
use quant_core::Claim;
use quant_core::ClaimId;
use quant_core::ClaimKind;
use quant_core::ClaimStatus;
use quant_core::Class;
use quant_core::ClientId;
use quant_core::RecordId;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::params;
use rusqlite::types::Value;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::Result;
use crate::Store;
use crate::StoreError;
use crate::from_micros;
use crate::micros;
use crate::records::fts_query;

/// A filter over stored claims.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClaimFilter {
	/// Only this status.
	#[serde(default)]
	pub status: Option<ClaimStatus>,
	/// Only this authority.
	#[serde(default)]
	pub authority: Option<Authority>,
	/// Only this kind.
	#[serde(default)]
	pub kind: Option<ClaimKind>,
	/// Only this subject.
	#[serde(default)]
	pub subject: Option<String>,
	/// Only this topic.
	#[serde(default)]
	pub topic: Option<String>,
	/// Never claims carrying one of these classes.
	#[serde(default)]
	pub exclude_classes: Vec<Class>,
	/// Most claims to return (0 means all).
	#[serde(default)]
	pub limit: usize,
}

impl ClaimFilter {
	/// A filter over nothing in particular.
	pub fn new() -> Self {
		Self::default()
	}

	/// Only claims with this status.
	#[must_use]
	pub fn status(mut self, status: ClaimStatus) -> Self {
		self.status = Some(status);
		self
	}

	/// Only claims with this authority.
	#[must_use]
	pub fn authority(mut self, authority: Authority) -> Self {
		self.authority = Some(authority);
		self
	}

	/// Only claims of this kind.
	#[must_use]
	pub fn kind(mut self, kind: ClaimKind) -> Self {
		self.kind = Some(kind);
		self
	}

	/// Only claims about this subject.
	#[must_use]
	pub fn subject(mut self, subject: impl Into<String>) -> Self {
		self.subject = Some(subject.into());
		self
	}

	/// Only claims with this topic.
	#[must_use]
	pub fn topic(mut self, topic: impl Into<String>) -> Self {
		self.topic = Some(topic.into());
		self
	}

	/// Never claims carrying one of these classes.
	#[must_use]
	pub fn exclude_classes(mut self, classes: &[Class]) -> Self {
		self.exclude_classes = classes.to_vec();
		self
	}

	/// At most this many claims.
	#[must_use]
	pub fn limit(mut self, limit: usize) -> Self {
		self.limit = limit;
		self
	}
}

/// One stored claim's columns, in table order after `rowid`.
pub(crate) const COLUMNS: &str = "c.id, c.subject, c.kind, c.statement, c.topic, c.authority, \
                                  c.confidence, c.classes, c.status, c.valid_from_us, \
                                  c.valid_to_us, c.proposed_by, c.created_at_us, c.updated_at_us, \
                                  c.superseded_by";

pub(crate) fn claim_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Claim>> {
	let id: Vec<u8> = row.get(0)?;
	let subject: String = row.get(1)?;
	let kind: String = row.get(2)?;
	let statement: String = row.get(3)?;
	let topic: Option<String> = row.get(4)?;
	let authority: String = row.get(5)?;
	let confidence: f32 = row.get(6)?;
	let classes: String = row.get(7)?;
	let status: String = row.get(8)?;
	let valid_from: Option<i64> = row.get(9)?;
	let valid_to: Option<i64> = row.get(10)?;
	let proposed_by: Option<Vec<u8>> = row.get(11)?;
	let created: i64 = row.get(12)?;
	let updated: i64 = row.get(13)?;
	let superseded_by: Option<Vec<u8>> = row.get(14)?;
	Ok((|| {
		Ok(Claim {
			id: ClaimId(Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("claim id"))?),
			subject,
			kind: ClaimKind::parse(&kind).ok_or(StoreError::Corrupt("claim kind"))?,
			statement,
			topic,
			authority: Authority::parse(&authority)
				.ok_or(StoreError::Corrupt("claim authority"))?,
			confidence,
			classes: serde_json::from_str(&classes)?,
			status: ClaimStatus::parse(&status).ok_or(StoreError::Corrupt("claim status"))?,
			valid_from: valid_from.map(from_micros).transpose()?,
			valid_to: valid_to.map(from_micros).transpose()?,
			proposed_by: proposed_by
				.map(|bytes| {
					Uuid::from_slice(&bytes)
						.map(ClientId)
						.map_err(|_| StoreError::Corrupt("claim proposer"))
				})
				.transpose()?,
			created_at: from_micros(created)?,
			updated_at: from_micros(updated)?,
			superseded_by: superseded_by
				.map(|bytes| {
					Uuid::from_slice(&bytes)
						.map(ClaimId)
						.map_err(|_| StoreError::Corrupt("claim superseder"))
				})
				.transpose()?,
			evidence: Vec::new(),
		})
	})())
}

pub(crate) fn filter_sql(filter: &ClaimFilter, params: &mut Vec<Value>) -> String {
	let mut clauses = Vec::new();
	if let Some(status) = filter.status {
		params.push(status.as_str().to_owned().into());
		clauses.push("status = ?".to_owned());
	}
	if let Some(authority) = filter.authority {
		params.push(authority.as_str().to_owned().into());
		clauses.push("authority = ?".to_owned());
	}
	if let Some(kind) = filter.kind {
		params.push(kind.as_str().to_owned().into());
		clauses.push("kind = ?".to_owned());
	}
	if let Some(subject) = &filter.subject {
		params.push(subject.clone().into());
		clauses.push("subject = ?".to_owned());
	}
	if let Some(topic) = &filter.topic {
		params.push(topic.clone().into());
		clauses.push("topic = ?".to_owned());
	}
	for class in &filter.exclude_classes {
		params.push(format!("%\"{}\"%", class.as_str()).into());
		clauses.push("classes NOT LIKE ?".to_owned());
	}
	if clauses.is_empty() {
		"1".to_owned()
	} else {
		format!("({})", clauses.join(" AND "))
	}
}

pub(crate) fn attach_evidence(conn: &rusqlite::Connection, claims: &mut [Claim]) -> Result<()> {
	let mut statement = conn.prepare_cached(
		"SELECT r.id FROM claim_evidence ce JOIN records r ON r.rowid = ce.record
		 WHERE ce.claim IN (SELECT rowid FROM claims WHERE id = ?1)",
	)?;
	for claim in claims {
		let rows = statement.query_map([claim.id.0.as_bytes().as_slice()], |row| {
			row.get::<_, Vec<u8>>(0)
		})?;
		for record in rows {
			let record = record?;
			claim.evidence.push(RecordId(
				Uuid::from_slice(&record).map_err(|_| StoreError::Corrupt("evidence record"))?,
			));
		}
	}
	Ok(())
}

impl Store {
	/// Store a claim and its evidence. Returns the claim's rowid.
	pub fn insert_claim(&self, claim: &Claim) -> Result<i64> {
		claim
			.validate()
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let classes = serde_json::to_string(&claim.classes)?;
		let rowid = {
			tx.execute(
				"INSERT INTO claims (id, subject, kind, statement, topic, authority, confidence, \
				 classes, status, valid_from_us, valid_to_us, proposed_by, created_at_us, \
				 updated_at_us, superseded_by)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
				params![
					claim.id.0.as_bytes().as_slice(),
					claim.subject,
					claim.kind.as_str(),
					claim.statement,
					claim.topic,
					claim.authority.as_str(),
					claim.confidence,
					classes,
					claim.status.as_str(),
					claim.valid_from.map(micros),
					claim.valid_to.map(micros),
					claim.proposed_by.map(|c| c.0.as_bytes().to_vec()),
					micros(claim.created_at),
					micros(claim.updated_at),
					claim.superseded_by.map(|c| c.0.as_bytes().to_vec()),
				],
			)?;
			tx.last_insert_rowid()
		};
		{
			let mut insert = tx.prepare_cached(
				"INSERT INTO claim_evidence (claim, record)
				 SELECT ?1, rowid FROM records WHERE id = ?2",
			)?;
			let mut linked = 0;
			for evidence in &claim.evidence {
				linked += insert.execute(params![rowid, evidence.0.as_bytes().as_slice()])?;
			}
			if linked != claim.evidence.len() {
				return Err(StoreError::NotFound);
			}
		}
		tx.commit()?;
		Ok(rowid)
	}

	/// One claim by id, with its evidence.
	pub fn claim(&self, id: ClaimId) -> Result<Option<Claim>> {
		let conn = self.conn();
		let found = conn
			.query_row(
				&format!("SELECT {COLUMNS} FROM claims c WHERE c.id = ?1"),
				[id.0.as_bytes().as_slice()],
				claim_from_row,
			)
			.optional()?
			.transpose()?;
		if let Some(mut claim) = found {
			attach_evidence(&conn, std::slice::from_mut(&mut claim))?;
			Ok(Some(claim))
		} else {
			Ok(None)
		}
	}

	/// Claims matching `filter`, newest first, with their evidence.
	pub fn claims(&self, filter: &ClaimFilter) -> Result<Vec<Claim>> {
		self.query_claims(filter, None)
	}

	/// Claims matching `filter` whose statement matches `terms`, best match
	/// first, with their evidence. Terms are quoted exactly like record
	/// search terms.
	pub fn search_claims(&self, filter: &ClaimFilter, terms: &str) -> Result<Vec<Claim>> {
		let Some(query) = fts_query(terms) else {
			return Ok(Vec::new());
		};
		self.claims_matching(filter, &query)
	}

	/// Claims matching `filter` whose statement matches the FTS query
	/// `query`, best match first. `query` must have been built by
	/// [`fts_query`] or [`fts_any`], never by interpolating caller text.
	pub fn claims_matching(&self, filter: &ClaimFilter, query: &str) -> Result<Vec<Claim>> {
		self.query_claims(filter, Some(query))
	}

	fn query_claims(&self, filter: &ClaimFilter, fts: Option<&str>) -> Result<Vec<Claim>> {
		let mut params: Vec<Value> = Vec::new();
		let (from, order, extra) = if let Some(query) = fts {
			params.push(query.to_owned().into());
			(
				"claims_fts f JOIN claims c ON c.rowid = f.rowid",
				"rank",
				"claims_fts MATCH ? AND",
			)
		} else {
			("claims c", "c.updated_at_us DESC", "")
		};
		let condition = filter_sql(filter, &mut params);
		if filter.limit > 0 {
			params.push(i64::try_from(filter.limit).unwrap_or(i64::MAX).into());
		}
		let limit = if filter.limit > 0 { "LIMIT ?" } else { "" };
		let sql = format!(
			"SELECT {COLUMNS} FROM {from} WHERE {extra} {condition} ORDER BY {order} {limit}"
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(rusqlite::params_from_iter(params), claim_from_row)?;
		let mut claims = Vec::new();
		for row in rows {
			claims.push(row??);
		}
		attach_evidence(&conn, &mut claims)?;
		Ok(claims)
	}

	/// Change a claim's status, touching its `updated_at`.
	pub fn set_claim_status(&self, id: ClaimId, status: ClaimStatus) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE claims SET status = ?2, updated_at_us = ?3 WHERE id = ?1",
			params![
				id.0.as_bytes().as_slice(),
				status.as_str(),
				micros(Timestamp::now())
			],
		)?;
		if changed == 0 {
			Err(StoreError::NotFound)
		} else {
			Ok(())
		}
	}

	/// Change a claim's confidence, touching its `updated_at`.
	pub fn set_claim_confidence(&self, id: ClaimId, confidence: f32) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE claims SET confidence = ?2, updated_at_us = ?3 WHERE id = ?1",
			params![
				id.0.as_bytes().as_slice(),
				confidence,
				micros(Timestamp::now())
			],
		)?;
		if changed == 0 {
			Err(StoreError::NotFound)
		} else {
			Ok(())
		}
	}

	/// Add evidence records to a claim, ignoring ones it already cites.
	pub fn add_claim_evidence(&self, id: ClaimId, evidence: &[RecordId]) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		{
			let mut insert = tx.prepare_cached(
				"INSERT OR IGNORE INTO claim_evidence (claim, record)
				 SELECT (SELECT rowid FROM claims WHERE id = ?1), rowid FROM records WHERE id = ?2",
			)?;
			for record in evidence {
				insert.execute(params![
					id.0.as_bytes().as_slice(),
					record.0.as_bytes().as_slice()
				])?;
			}
		}
		tx.commit()?;
		Ok(())
	}

	/// Mark `old` as superseded by `new`.
	pub fn supersede(&self, old: ClaimId, new: ClaimId) -> Result<()> {
		let changed = self.conn().execute(
			"UPDATE claims SET status = 'superseded', superseded_by = ?2, updated_at_us = ?3
			 WHERE id = ?1",
			params![
				old.0.as_bytes().as_slice(),
				new.0.as_bytes().as_slice(),
				micros(Timestamp::now())
			],
		)?;
		if changed == 0 {
			Err(StoreError::NotFound)
		} else {
			Ok(())
		}
	}

	/// How many proposals `client` made at or after `since`.
	pub fn proposal_count(&self, client: ClientId, since: Timestamp) -> Result<u64> {
		let count: i64 = self.conn().query_row(
			"SELECT count(*) FROM claims WHERE proposed_by = ?1 AND created_at_us >= ?2",
			params![client.0.as_bytes().as_slice(), micros(since)],
			|r| r.get(0),
		)?;
		Ok(u64::try_from(count).unwrap_or(0))
	}

	/// Delete claims with authority Observed or Inferred whose evidence rows
	/// were just removed by a purge. Claims the owner made are never deleted
	/// by a purge. `had_evidence` is the set of claim rowids that had
	/// evidence before the purge ran.
	pub(crate) fn prune_claims_after_purge(
		tx: &rusqlite::Transaction<'_>,
		had_evidence: &[i64],
	) -> Result<()> {
		let mut delete = tx.prepare_cached(
			"DELETE FROM claims WHERE rowid = ?1
			 AND authority IN ('observed', 'inferred')
			 AND NOT EXISTS (SELECT 1 FROM claim_evidence WHERE claim = ?1)",
		)?;
		for rowid in had_evidence {
			delete.execute([rowid])?;
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;
	use quant_core::DeviceId;
	use quant_core::Record;
	use quant_core::TimeRange;

	use super::*;
	use crate::Selector;
	use crate::test_support::store;

	fn at(minutes: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_mins(minutes)
	}

	fn record(minute: i64, text: &str) -> Record {
		Record::new("screen.frame".parse().unwrap(), DeviceId::new())
			.starting(at(minute))
			.with_text(text)
	}

	fn claim(statement: &str, authority: Authority) -> Claim {
		Claim::new(statement, ClaimKind::Preference, authority)
	}

	#[test]
	fn stores_reads_and_lists_claims_with_evidence() {
		let store = store();
		let records = store
			.insert_records(&[
				record(1, "ordered an espresso at 14:02"),
				record(2, "pulled the quarterly budget"),
			])
			.unwrap();
		assert_eq!(records.stored, 2);

		let mut owner = claim("Prefers espresso after lunch", Authority::Owner);
		owner.topic = Some("coffee.timing".into());
		owner.confidence = 0.9;
		store.insert_claim(&owner).unwrap();

		let inferred = Claim {
			id: ClaimId::new(),
			subject: "me".into(),
			kind: ClaimKind::Routine,
			statement: "Writes in the afternoon".into(),
			topic: None,
			authority: Authority::Inferred,
			confidence: 0.6,
			classes: vec![Class::Health],
			status: ClaimStatus::Held,
			valid_from: None,
			valid_to: None,
			evidence: Vec::new(),
			proposed_by: Some(ClientId::new()),
			created_at: Timestamp::now(),
			updated_at: Timestamp::now(),
			superseded_by: None,
		};
		// No evidence yet: the store fills it from the records table.
		let all = store
			.records(&crate::RecordQuery::new(
				Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap();
		let mut with_evidence = inferred.clone();
		with_evidence.evidence = vec![all[1].id];
		store.insert_claim(&with_evidence).unwrap();

		let back = store.claim(owner.id).unwrap().unwrap();
		assert_eq!(back.statement, "Prefers espresso after lunch");
		assert_eq!(back.topic.as_deref(), Some("coffee.timing"));
		assert!(back.evidence.is_empty());

		let active = store
			.claims(&ClaimFilter::new().status(ClaimStatus::Active))
			.unwrap();
		assert_eq!(active.len(), 1);
		assert_eq!(active[0].id, owner.id);

		let held = store
			.claims(&ClaimFilter::new().status(ClaimStatus::Held))
			.unwrap();
		assert_eq!(held.len(), 1);
		assert_eq!(held[0].evidence, vec![all[1].id]);
		assert_eq!(held[0].proposed_by, with_evidence.proposed_by);

		let by_topic = store
			.claims(&ClaimFilter::new().topic("coffee.timing"))
			.unwrap();
		assert_eq!(by_topic.len(), 1);

		let excluding = store
			.claims(&ClaimFilter::new().exclude_classes(&[Class::Health]))
			.unwrap();
		assert_eq!(excluding.len(), 1);
		assert_eq!(excluding[0].id, owner.id);

		let limited = store.claims(&ClaimFilter::new().limit(1)).unwrap();
		assert_eq!(limited.len(), 1);
	}

	#[test]
	fn claim_search_quotes_terms_and_finds_matches() {
		let store = store();
		for (statement, authority) in [
			("Prefers espresso after lunch", Authority::Owner),
			("Drinks tea in the morning", Authority::Owner),
			(
				"Prepping for the quarterly review",
				Authority::OwnerAuthored,
			),
		] {
			store.insert_claim(&claim(statement, authority)).unwrap();
		}
		let hits = store
			.search_claims(&ClaimFilter::new(), "espresso lunch")
			.unwrap();
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].statement, "Prefers espresso after lunch");
		// FTS operators in terms are quoted, never interpreted.
		assert!(
			store
				.search_claims(&ClaimFilter::new(), "NEAR( \"x")
				.is_ok()
		);
		assert!(
			store
				.search_claims(&ClaimFilter::new(), "*** --")
				.unwrap()
				.is_empty()
		);
	}

	#[test]
	fn statuses_and_supersession() {
		let store = store();
		let first = claim("Likes dark roast", Authority::OwnerAuthored);
		store.insert_claim(&first).unwrap();
		let second = claim("Prefers a light roast now", Authority::Owner);
		store.insert_claim(&second).unwrap();

		store.set_claim_status(first.id, ClaimStatus::Held).unwrap();
		assert_eq!(
			store.claim(first.id).unwrap().unwrap().status,
			ClaimStatus::Held
		);

		store.supersede(first.id, second.id).unwrap();
		let superseded = store.claim(first.id).unwrap().unwrap();
		assert_eq!(superseded.status, ClaimStatus::Superseded);
		assert_eq!(superseded.superseded_by, Some(second.id));

		assert!(matches!(
			store.set_claim_status(ClaimId::new(), ClaimStatus::Active),
			Err(StoreError::NotFound)
		));
	}

	#[test]
	fn purging_evidence_records_prunes_only_derived_claims() {
		let store = store();
		let evidence = store
			.insert_records(&[record(1, "the evidence"), record(2, "more evidence")])
			.unwrap();
		assert_eq!(evidence.stored, 2);
		let records = store
			.records(&crate::RecordQuery::new(
				Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap();
		let all_ids: Vec<_> = records.iter().map(|r| r.id).collect();

		let mut observed = claim("Observed a pattern", Authority::Observed);
		observed.evidence = vec![all_ids[0]];
		store.insert_claim(&observed).unwrap();

		let mut owner = claim("Owner knows best", Authority::Owner);
		owner.evidence = vec![all_ids[1]];
		store.insert_claim(&owner).unwrap();

		// Purging the evidence cascades out of claim_evidence; the observed
		// claim goes with it, the owner's stays.
		store
			.purge(&Selector::all(), TimeRange::new(at(0), at(10)).unwrap())
			.unwrap();
		assert!(store.claim(observed.id).unwrap().is_none());
		let kept = store.claim(owner.id).unwrap().unwrap();
		assert_eq!(kept.status, ClaimStatus::Active);
		assert!(kept.evidence.is_empty());
	}

	#[test]
	fn deleting_a_record_cascades_out_of_claim_evidence() {
		let store = store();
		let record = store.insert_records(&[record(1, "grounding")]).unwrap();
		assert_eq!(record.stored, 1);
		let records = store
			.records(&crate::RecordQuery::new(
				Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap();

		let mut inferred = claim("Agent said so", Authority::Inferred);
		inferred.evidence = vec![records[0].id];
		let rowid = store.insert_claim(&inferred).unwrap();

		let evidence_rows: i64 = store
			.conn()
			.query_row(
				"SELECT count(*) FROM claim_evidence WHERE claim = ?1",
				[rowid],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(evidence_rows, 1);

		store.delete_record_rows(&[1]).unwrap();
		let evidence_rows: i64 = store
			.conn()
			.query_row(
				"SELECT count(*) FROM claim_evidence WHERE claim = ?1",
				[rowid],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(evidence_rows, 0);
	}

	#[test]
	fn proposals_are_counted_per_hour() {
		let store = store();
		let client = ClientId::new();
		let now = Timestamp::now();
		store
			.insert_records(&[record(1, "grounding for the proposal")])
			.unwrap();
		let records = store
			.records(&crate::RecordQuery::new(
				Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap();

		let mut claim = claim("A proposal", Authority::Inferred);
		claim.status = ClaimStatus::Held;
		claim.proposed_by = Some(client);
		claim.evidence = vec![records[0].id];
		store.insert_claim(&claim).unwrap();

		assert_eq!(
			store
				.proposal_count(client, now - SignedDuration::from_hours(1))
				.unwrap(),
			1
		);
		assert_eq!(
			store
				.proposal_count(client, now + SignedDuration::from_mins(1))
				.unwrap(),
			0
		);
	}

	#[test]
	fn invalid_claims_are_rejected() {
		let store = store();
		assert!(matches!(
			store.insert_claim(&claim("", Authority::Owner)),
			Err(StoreError::Invalid(_))
		));
		let mut observed = claim("No grounding", Authority::Observed);
		assert!(matches!(
			store.insert_claim(&observed),
			Err(StoreError::Invalid(_))
		));
		// Evidence ids that name no record fail as not found.
		observed.evidence = vec![RecordId::new()];
		assert!(matches!(
			store.insert_claim(&observed),
			Err(StoreError::NotFound)
		));
	}
}
