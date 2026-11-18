//! The questions ledger: each question an agent asked of the vault, the
//! answer it gave, and what was served along the way.
//!
//! Questions and answers are the agent's own text about the owner, so they
//! are stored as given (inside the encrypted vault, like everything else)
//! and sanitised and redacted on the way out, exactly like other served
//! text. Only the owner and the client that asked ever see an answer.

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::audit::AuditEntry;
use quant_core::ClientId;
use quant_core::GrantId;
use quant_core::QuestionId;
use quant_core::RecordId;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::params;
use uuid::Uuid;

use crate::Result;
use crate::Store;
use crate::StoreError;
use crate::from_micros;
use crate::micros;

/// How long a question may sit unanswered before the vault abandons it.
pub const ANSWER_WINDOW: SignedDuration = SignedDuration::from_hours(24);

/// One stored question.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
	/// Its id.
	pub id: QuestionId,
	/// The client that asked.
	pub client: ClientId,
	/// The grant it asked under.
	pub grant: GrantId,
	/// When it was asked.
	pub asked_at: Timestamp,
	/// The question, as asked.
	pub question: String,
	/// The purpose given for it.
	pub purpose: String,
	/// The model that ran the session, when it said.
	pub model: Option<String>,
	/// Where it stands.
	pub status: QuestionStatus,
	/// The answer, once given.
	pub answer: Option<String>,
	/// When it was answered.
	pub answered_at: Option<Timestamp>,
	/// Records the answer cites.
	pub citations: Vec<RecordId>,
	/// Records served under this question.
	pub records: u64,
	/// Bytes served under this question.
	pub bytes: u64,
}

/// Where a question stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionStatus {
	/// Opened, not yet answered.
	Open,
	/// Answered.
	Answered,
	/// Left unanswered past the window and closed by the vault.
	Abandoned,
}

impl QuestionStatus {
	/// The stored name.
	pub fn as_str(self) -> &'static str {
		match self {
			QuestionStatus::Open => "open",
			QuestionStatus::Answered => "answered",
			QuestionStatus::Abandoned => "abandoned",
		}
	}

	fn parse(s: &str) -> Result<Self> {
		match s {
			"open" => Ok(QuestionStatus::Open),
			"answered" => Ok(QuestionStatus::Answered),
			"abandoned" => Ok(QuestionStatus::Abandoned),
			_ => Err(StoreError::Corrupt("question status")),
		}
	}
}

const COLUMNS: &str = "q.id, q.client, q.grant, q.asked_us, q.question, q.purpose, q.model, \
                       q.status, q.answer, q.answered_us, q.records, q.bytes";

fn question_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Question>> {
	let id: Vec<u8> = row.get(0)?;
	let client: Vec<u8> = row.get(1)?;
	let grant: Vec<u8> = row.get(2)?;
	let asked: i64 = row.get(3)?;
	let question: String = row.get(4)?;
	let purpose: String = row.get(5)?;
	let model: Option<String> = row.get(6)?;
	let status: String = row.get(7)?;
	let answer: Option<String> = row.get(8)?;
	let answered: Option<i64> = row.get(9)?;
	let records: i64 = row.get(10)?;
	let bytes: i64 = row.get(11)?;
	Ok((|| {
		Ok(Question {
			id: QuestionId(Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("question id"))?),
			client: ClientId(
				Uuid::from_slice(&client).map_err(|_| StoreError::Corrupt("question client"))?,
			),
			grant: GrantId(
				Uuid::from_slice(&grant).map_err(|_| StoreError::Corrupt("question grant"))?,
			),
			asked_at: from_micros(asked)?,
			question,
			purpose,
			model,
			status: QuestionStatus::parse(&status)?,
			answer,
			answered_at: answered.map(from_micros).transpose()?,
			records: u64::try_from(records).unwrap_or(0),
			bytes: u64::try_from(bytes).unwrap_or(0),
			citations: Vec::new(),
		})
	})())
}

fn attach_citations(conn: &rusqlite::Connection, questions: &mut [Question]) -> Result<()> {
	let mut statement = conn.prepare_cached(
		"SELECT r.id FROM question_citations qc JOIN records r ON r.rowid = qc.record
		 WHERE qc.question IN (SELECT rowid FROM questions WHERE id = ?1)",
	)?;
	for question in questions {
		let rows = statement.query_map([question.id.0.as_bytes().as_slice()], |row| {
			row.get::<_, Vec<u8>>(0)
		})?;
		for record in rows {
			let record = record?;
			question.citations.push(RecordId(
				Uuid::from_slice(&record).map_err(|_| StoreError::Corrupt("cited record"))?,
			));
		}
	}
	Ok(())
}

impl Store {
	/// Close every question older than the answer window that is still
	/// open. Answers about a day-old question are worth less than an
	/// honest gap, and an abandoned question stops accepting reads.
	pub fn abandon_stale_questions(&self, now: Timestamp) -> Result<u64> {
		let cutoff = micros(now - ANSWER_WINDOW);
		let changed = self.conn().execute(
			"UPDATE questions SET status = 'abandoned' WHERE status = 'open' AND asked_us < ?1",
			[cutoff],
		)?;
		Ok(u64::try_from(changed).unwrap_or(0))
	}

	/// Open a question. The stored text is the caller's own words; the
	/// daemon sanitises and length-checks before it gets here.
	pub fn insert_question(&self, question: &Question) -> Result<()> {
		self.conn().execute(
			"INSERT INTO questions (id, client, grant, asked_us, question, purpose, model, \
			 status, answer, answered_us, records, bytes)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, 0, 0)",
			params![
				question.id.0.as_bytes().as_slice(),
				question.client.0.as_bytes().as_slice(),
				question.grant.0.as_bytes().as_slice(),
				micros(question.asked_at),
				question.question,
				question.purpose,
				question.model,
				question.status.as_str(),
			],
		)?;
		Ok(())
	}

	/// One question by id, with its citations.
	pub fn question(&self, id: QuestionId) -> Result<Option<Question>> {
		let conn = self.conn();
		let found = conn
			.query_row(
				&format!("SELECT {COLUMNS} FROM questions q WHERE q.id = ?1"),
				[id.0.as_bytes().as_slice()],
				question_from_row,
			)
			.optional()?
			.transpose()?;
		if let Some(mut question) = found {
			attach_citations(&conn, std::slice::from_mut(&mut question))?;
			Ok(Some(question))
		} else {
			Ok(None)
		}
	}

	/// Questions matching `client` (or every client when `None`), newest
	/// first, with their citations.
	pub fn questions(&self, client: Option<ClientId>, limit: usize) -> Result<Vec<Question>> {
		let mut sql = format!("SELECT {COLUMNS} FROM questions q");
		let mut bound: Vec<rusqlite::types::Value> = Vec::new();
		if let Some(client) = client {
			sql.push_str(" WHERE q.client = ?");
			bound.push(client.0.as_bytes().to_vec().into());
		}
		sql.push_str(" ORDER BY q.asked_us DESC LIMIT ?");
		bound.push(i64::try_from(limit).unwrap_or(i64::MAX).into());
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(rusqlite::params_from_iter(bound), question_from_row)?;
		let mut questions = Vec::new();
		for row in rows {
			questions.push(row??);
		}
		attach_citations(&conn, &mut questions)?;
		Ok(questions)
	}

	/// Record `client`'s answer to its question, with citations. Every
	/// cited record must exist; the daemon has already checked the grant
	/// could read each one. Returns `NotFound` when the question is not
	/// `client`'s to answer, already answered, or abandoned — never saying
	/// which.
	pub fn answer_question(
		&self,
		id: QuestionId,
		client: ClientId,
		answer: &str,
		citations: &[RecordId],
		now: Timestamp,
	) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let question: Option<i64> = tx
			.query_row(
				"SELECT rowid FROM questions WHERE id = ?1 AND client = ?2",
				params![id.0.as_bytes().as_slice(), client.0.as_bytes().as_slice()],
				|r| r.get(0),
			)
			.optional()?;
		let Some(rowid) = question else {
			return Err(StoreError::NotFound);
		};
		let changed = tx.execute(
			"UPDATE questions SET answer = ?3, answered_us = ?4, status = 'answered'
			 WHERE rowid = ?1 AND status = 'open'",
			params![rowid, client.0.as_bytes().as_slice(), answer, micros(now)],
		)?;
		if changed == 0 {
			return Err(StoreError::NotFound);
		}
		{
			let mut insert = tx.prepare_cached(
				"INSERT INTO question_citations (question, record)
				 SELECT ?1, rowid FROM records WHERE id = ?2",
			)?;
			let mut linked = 0;
			for citation in citations {
				linked += insert.execute(params![rowid, citation.0.as_bytes().as_slice()])?;
			}
			if linked != citations.len() {
				return Err(StoreError::NotFound);
			}
		}
		tx.commit()?;
		Ok(())
	}

	/// Add to a question's totals of what was served under it.
	pub fn charge_question(&self, id: QuestionId, records: u64, bytes: u64) -> Result<()> {
		self.conn().execute(
			"UPDATE questions SET records = records + ?2, bytes = bytes + ?3 WHERE id = ?1",
			params![
				id.0.as_bytes().as_slice(),
				i64::try_from(records).unwrap_or(i64::MAX),
				i64::try_from(bytes).unwrap_or(i64::MAX)
			],
		)?;
		Ok(())
	}

	/// How many questions `grant` opened at or after `since`.
	pub fn question_count(&self, grant: GrantId, since: Timestamp) -> Result<u64> {
		let count: i64 = self.conn().query_row(
			"SELECT count(*) FROM questions WHERE grant = ?1 AND asked_us >= ?2",
			params![grant.0.as_bytes().as_slice(), micros(since)],
			|r| r.get(0),
		)?;
		Ok(u64::try_from(count).unwrap_or(0))
	}

	/// Every audit entry carrying `id` as its question, oldest first. The
	/// question field lives inside the hashed entry JSON, so the trail is
	/// exactly the entries the chain covers. Ids compare in their raw
	/// serde form — a bare UUID — matching how entries serialise.
	pub fn audit_for_question(&self, id: QuestionId, limit: usize) -> Result<Vec<AuditEntry>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(
			"SELECT entry FROM audit
			 WHERE json_extract(entry, '$.question') = ?1
			 ORDER BY seq LIMIT ?2",
		)?;
		let rows = statement.query_map(
			params![id.0.to_string(), i64::try_from(limit).unwrap_or(i64::MAX)],
			|r| r.get::<_, String>(0),
		)?;
		rows.map(|entry| Ok(serde_json::from_str(&entry?)?))
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;
	use quant_access::audit::Actor;
	use quant_access::audit::AuditEvent;
	use quant_access::audit::Outcome;
	use quant_core::DeviceId;
	use quant_core::Record;
	use quant_core::TimeRange;

	use super::*;
	use crate::test_support::store;

	fn at(minutes: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_mins(minutes)
	}

	fn question(client: ClientId, at: Timestamp) -> Question {
		Question {
			id: QuestionId::new(),
			client,
			grant: GrantId::new(),
			asked_at: at,
			question: "How did I sleep after late coffee?".into(),
			purpose: "answering the owner's question about sleep".into(),
			model: Some("claude-opus-5-5".into()),
			status: QuestionStatus::Open,
			answer: None,
			answered_at: None,
			citations: Vec::new(),
			records: 0,
			bytes: 0,
		}
	}

	/// A client row to ask under, since questions point at real clients.
	fn asker(store: &Store) -> ClientId {
		let client = store
			.register_client(
				"Claude",
				crate::ClientKind::Agent,
				quant_access::auth::ClientKey::generate().public(),
				&quant_access::Scopes::default(),
				Some(&quant_access::AgentIdentity {
					product: "Claude Code".into(),
					vendor: "Anthropic".into(),
					model: "claude-opus-5-5".into(),
					version: "2.1.0".into(),
					host: "mini".into(),
					operator: None,
				}),
			)
			.unwrap();
		client.id
	}

	fn record(minute: i64, text: &str) -> Record {
		Record::new("screen.frame".parse().unwrap(), DeviceId::new())
			.starting(at(minute))
			.with_text(text)
	}

	#[test]
	fn opens_lists_and_answers_questions() {
		let store = store();
		let client = asker(&store);
		let opened = question(client, at(0));
		store.insert_question(&opened).unwrap();

		let back = store.question(opened.id).unwrap().unwrap();
		assert_eq!(back.question, "How did I sleep after late coffee?");
		assert_eq!(back.status, QuestionStatus::Open);
		assert!(back.citations.is_empty());

		let other = question(asker(&store), at(1));
		store.insert_question(&other).unwrap();
		let listed = store.questions(None, 10).unwrap();
		assert_eq!(listed.len(), 2);
		assert_eq!(listed[0].id, other.id, "newest first");
		assert_eq!(store.questions(Some(client), 10).unwrap().len(), 1);
		assert_eq!(store.questions(None, 1).unwrap().len(), 1);

		let records = store
			.insert_records(&[record(2, "slept badly"), record(3, "coffee at 21:00")])
			.unwrap();
		assert_eq!(records.stored, 2);
		let cited: Vec<RecordId> = store
			.records(&crate::RecordQuery::new(
				crate::Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap()
			.iter()
			.map(|r| r.id)
			.collect();
		store
			.answer_question(
				opened.id,
				client,
				"Badly: 5.2 hours, waking twice.",
				&cited,
				at(5),
			)
			.unwrap();
		let answered = store.question(opened.id).unwrap().unwrap();
		assert_eq!(answered.status, QuestionStatus::Answered);
		assert_eq!(
			answered.answer.as_deref(),
			Some("Badly: 5.2 hours, waking twice.")
		);
		assert_eq!(answered.answered_at, Some(at(5)));
		let mut served = answered.citations.clone();
		served.sort();
		let mut expected = cited.clone();
		expected.sort();
		assert_eq!(served, expected);

		// Only the owner of the question, and only once.
		assert!(matches!(
			store.answer_question(opened.id, ClientId::new(), "mine", &[], at(6)),
			Err(StoreError::NotFound)
		));
		assert!(matches!(
			store.answer_question(opened.id, client, "again", &[], at(6)),
			Err(StoreError::NotFound)
		));
		// A citation that names no record fails whole.
		let fresh = question(client, at(7));
		store.insert_question(&fresh).unwrap();
		assert!(matches!(
			store.answer_question(fresh.id, client, "grounded", &[RecordId::new()], at(8)),
			Err(StoreError::NotFound)
		));
	}

	#[test]
	fn open_questions_expire_after_a_day() {
		let store = store();
		let client = asker(&store);
		let stale = question(client, at(0));
		let fresh = question(client, at(60 * 25));
		store.insert_question(&stale).unwrap();
		store.insert_question(&fresh).unwrap();

		assert_eq!(store.abandon_stale_questions(at(60 * 26)).unwrap(), 1);
		assert_eq!(
			store.question(stale.id).unwrap().unwrap().status,
			QuestionStatus::Abandoned
		);
		assert_eq!(
			store.question(fresh.id).unwrap().unwrap().status,
			QuestionStatus::Open
		);
		// Abandoned questions no longer take answers.
		assert!(matches!(
			store.answer_question(stale.id, client, "late", &[], at(60 * 26)),
			Err(StoreError::NotFound)
		));
	}

	#[test]
	fn charges_and_counts_per_grant() {
		let store = store();
		let client = asker(&store);
		let a = question(client, at(0));
		store.insert_question(&a).unwrap();
		store.charge_question(a.id, 12, 3_000).unwrap();
		store.charge_question(a.id, 3, 500).unwrap();
		let back = store.question(a.id).unwrap().unwrap();
		assert_eq!((back.records, back.bytes), (15, 3_500));
		assert_eq!(
			store
				.question_count(a.grant, at(0) - SignedDuration::from_hours(1))
				.unwrap(),
			1
		);
		assert_eq!(store.question_count(GrantId::new(), at(0)).unwrap(), 0);
	}

	#[test]
	fn the_trail_finds_only_its_own_entries() {
		let store = store();
		let mine = QuestionId::new();
		let theirs = QuestionId::new();
		for (seq, id) in [
			(1, None),
			(2, Some(mine)),
			(3, Some(theirs)),
			(4, Some(mine)),
		] {
			let event = AuditEvent::new(
				Actor::System,
				&format!("query{seq}"),
				"health.sleep",
				Outcome::Allowed {
					records: seq,
					bytes: 0,
				},
			)
			.meta(
				Some("answering the owner's question about sleep"),
				id,
				Some("claude-opus-5-5"),
			);
			store.audit(event).unwrap();
		}
		let trail = store.audit_for_question(mine, 100).unwrap();
		assert_eq!(trail.len(), 2);
		assert_eq!(trail[0].action, "query2");
		assert_eq!(trail[1].action, "query4");
		assert!(trail.iter().all(|e| e.question == Some(mine)));
		assert!(trail.iter().all(|e| e.purpose.is_some()));
	}

	#[test]
	fn deleting_a_cited_record_cascades_out_of_the_ledger() {
		let store = store();
		let client = asker(&store);
		let opened = question(client, at(0));
		store.insert_question(&opened).unwrap();
		let stored = store.insert_records(&[record(1, "the citation")]).unwrap();
		assert_eq!(stored.stored, 1);
		let cited: Vec<RecordId> = store
			.records(&crate::RecordQuery::new(
				crate::Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			))
			.unwrap()
			.iter()
			.map(|r| r.id)
			.collect();
		store
			.answer_question(opened.id, client, "an answer", &cited, at(2))
			.unwrap();

		store
			.purge(
				&crate::Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			)
			.unwrap();
		let back = store.question(opened.id).unwrap().unwrap();
		assert_eq!(back.status, QuestionStatus::Answered);
		assert!(
			back.citations.is_empty(),
			"the citation went with its record"
		);
	}
}
