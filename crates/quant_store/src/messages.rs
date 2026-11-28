//! Companion messages: one separately approved companion device asks one
//! approved agent a question, and later reads the agent's actual reply.
//!
//! This is a mailbox, not the agent-opened questions ledger. The two live in
//! separate tables with separate routes, so neither can expose the other.
//! Message ids use the ledger's [`QuestionId`] shape so one id grammar covers
//! both; the tables and their meanings never mix.
//!
//! Messages and answers are text about the owner, so they are stored as given
//! inside the encrypted vault, exactly like questions, and sanitised and
//! redacted on the way out. A message can only be answered once, by its
//! recipient; the update is transactional, so two sessions of the same agent
//! cannot both reply.

use jiff::SignedDuration;
use jiff::Timestamp;
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

/// How long a companion message may sit unanswered before the vault closes
/// it. Longer than the 24-hour question window: a phone may leave a question
/// for an agent that is not running for several days.
pub const MESSAGE_WINDOW: SignedDuration = SignedDuration::from_hours(24 * 7);

/// One message from a companion device to an approved agent.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
	/// Its id, in the ledger's question-id form.
	pub id: QuestionId,
	/// The companion client that asked.
	pub requester: ClientId,
	/// Its name, as stored.
	pub requester_name: String,
	/// The agent client addressed.
	pub recipient: ClientId,
	/// Its name, as stored.
	pub recipient_name: String,
	/// The grant the requester asked under.
	pub grant: GrantId,
	/// When it was submitted.
	pub submitted_at: Timestamp,
	/// The question, as submitted.
	pub question: String,
	/// The optional purpose the requester gave.
	pub purpose: Option<String>,
	/// The requester's idempotency key.
	pub dedupe: String,
	/// Where it stands.
	pub status: MessageStatus,
	/// The agent's answer, once given.
	pub answer: Option<String>,
	/// When it was answered.
	pub answered_at: Option<Timestamp>,
	/// Records the answer cites.
	pub citations: Vec<RecordId>,
}

/// Where a companion message stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageStatus {
	/// Waiting for the addressed agent to pull it.
	Open,
	/// Answered by the addressed agent.
	Answered,
	/// Left unanswered past the window, or closed because either side's
	/// grant was revoked.
	Abandoned,
}

impl MessageStatus {
	/// The stored name.
	pub fn as_str(self) -> &'static str {
		match self {
			MessageStatus::Open => "open",
			MessageStatus::Answered => "answered",
			MessageStatus::Abandoned => "abandoned",
		}
	}

	fn parse(s: &str) -> Result<Self> {
		match s {
			"open" => Ok(MessageStatus::Open),
			"answered" => Ok(MessageStatus::Answered),
			"abandoned" => Ok(MessageStatus::Abandoned),
			_ => Err(StoreError::Corrupt("companion message status")),
		}
	}
}

const COLUMNS: &str = "m.id, m.requester, m.recipient, m.grant, m.submitted_us, m.question, \
                       m.purpose, m.dedupe, m.status, m.answer, m.answered_us, requester.name, \
                       recipient.name";

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Message>> {
	let id: Vec<u8> = row.get(0)?;
	let requester: Vec<u8> = row.get(1)?;
	let recipient: Vec<u8> = row.get(2)?;
	let grant: Vec<u8> = row.get(3)?;
	let submitted: i64 = row.get(4)?;
	let question: String = row.get(5)?;
	let purpose: Option<String> = row.get(6)?;
	let dedupe: String = row.get(7)?;
	let status: String = row.get(8)?;
	let answer: Option<String> = row.get(9)?;
	let answered: Option<i64> = row.get(10)?;
	let requester_name: String = row.get(11)?;
	let recipient_name: String = row.get(12)?;
	Ok((|| {
		Ok(Message {
			id: QuestionId(Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("message id"))?),
			requester: ClientId(
				Uuid::from_slice(&requester)
					.map_err(|_| StoreError::Corrupt("message requester"))?,
			),
			requester_name,
			recipient: ClientId(
				Uuid::from_slice(&recipient)
					.map_err(|_| StoreError::Corrupt("message recipient"))?,
			),
			recipient_name,
			grant: GrantId(
				Uuid::from_slice(&grant).map_err(|_| StoreError::Corrupt("message grant"))?,
			),
			submitted_at: from_micros(submitted)?,
			question,
			purpose,
			dedupe,
			status: MessageStatus::parse(&status)?,
			answer,
			answered_at: answered.map(from_micros).transpose()?,
			citations: Vec::new(),
		})
	})())
}

fn attach_citations(conn: &rusqlite::Connection, messages: &mut [Message]) -> Result<()> {
	let mut statement = conn.prepare_cached(
		"SELECT r.id FROM companion_citations cc JOIN records r ON r.rowid = cc.record
		 WHERE cc.message IN (SELECT rowid FROM companion_messages WHERE id = ?1)",
	)?;
	for message in messages {
		let rows = statement.query_map([message.id.0.as_bytes().as_slice()], |row| {
			row.get::<_, Vec<u8>>(0)
		})?;
		for record in rows {
			let record = record?;
			message.citations.push(RecordId(
				Uuid::from_slice(&record).map_err(|_| StoreError::Corrupt("cited record"))?,
			));
		}
	}
	Ok(())
}

impl Store {
	/// Close every companion message older than [`MESSAGE_WINDOW`] that is
	/// still open. A week-old question is better recorded as abandoned than
	/// answered late.
	pub fn abandon_stale_messages(&self, now: Timestamp) -> Result<u64> {
		let cutoff = micros(now - MESSAGE_WINDOW);
		let changed = self.conn().execute(
			"UPDATE companion_messages SET status = 'abandoned'
			 WHERE status = 'open' AND submitted_us < ?1",
			[cutoff],
		)?;
		Ok(u64::try_from(changed).unwrap_or(0))
	}

	/// Store a message. A message with the same requester and `dedupe` key
	/// already exists: return that one instead, so a retry after a lost
	/// response never creates a second question.
	pub fn insert_message(&self, message: &Message) -> Result<Message> {
		let mut conn = self.conn();
		let id = {
			let tx = conn.transaction()?;
			let existing: Option<Vec<u8>> = tx
				.query_row(
					"SELECT id FROM companion_messages WHERE requester = ?1 AND dedupe = ?2",
					params![
						message.requester.0.as_bytes().as_slice(),
						message.dedupe.as_str()
					],
					|row| row.get(0),
				)
				.optional()?;
			let id = if let Some(bytes) = existing {
				Uuid::from_slice(&bytes).map_err(|_| StoreError::Corrupt("message id"))?
			} else {
				tx.execute(
					"INSERT INTO companion_messages
					 (id, requester, recipient, grant, submitted_us, question, purpose,
					  dedupe, status, answer, answered_us)
					 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)",
					params![
						message.id.0.as_bytes().as_slice(),
						message.requester.0.as_bytes().as_slice(),
						message.recipient.0.as_bytes().as_slice(),
						message.grant.0.as_bytes().as_slice(),
						micros(message.submitted_at),
						message.question,
						message.purpose,
						message.dedupe,
						message.status.as_str(),
					],
				)?;
				message.id.0
			};
			tx.commit()?;
			id
		};
		drop(conn);
		self.message(QuestionId(id))?.ok_or(StoreError::NotFound)
	}

	/// One message by id, with its citations.
	pub fn message(&self, id: QuestionId) -> Result<Option<Message>> {
		let conn = self.conn();
		let found = conn
			.query_row(
				&format!(
					"SELECT {COLUMNS} FROM companion_messages m
					 JOIN clients requester ON requester.id = m.requester
					 JOIN clients recipient ON recipient.id = m.recipient
					 WHERE m.id = ?1"
				),
				[id.0.as_bytes().as_slice()],
				message_from_row,
			)
			.optional()?
			.transpose()?;
		if let Some(mut message) = found {
			attach_citations(&conn, std::slice::from_mut(&mut message))?;
			Ok(Some(message))
		} else {
			Ok(None)
		}
	}

	/// A requester's message by its idempotency key.
	pub fn message_by_dedupe(&self, requester: ClientId, dedupe: &str) -> Result<Option<Message>> {
		let conn = self.conn();
		let found = conn
			.query_row(
				&format!(
					"SELECT {COLUMNS} FROM companion_messages m
					 JOIN clients requester ON requester.id = m.requester
					 JOIN clients recipient ON recipient.id = m.recipient
					 WHERE m.requester = ?1 AND m.dedupe = ?2"
				),
				params![requester.0.as_bytes().as_slice(), dedupe],
				message_from_row,
			)
			.optional()?
			.transpose()?;
		if let Some(mut message) = found {
			attach_citations(&conn, std::slice::from_mut(&mut message))?;
			Ok(Some(message))
		} else {
			Ok(None)
		}
	}

	/// Messages `requester` sent, newest first, with their citations.
	pub fn messages_from(&self, requester: ClientId, limit: usize) -> Result<Vec<Message>> {
		self.select_messages(
			"m.requester = ?",
			requester.0.as_bytes(),
			"ORDER BY m.submitted_us DESC LIMIT ?",
			limit,
		)
	}

	/// Open messages addressed to `recipient`, oldest first: the agent's
	/// actionable inbox. Listing changes nothing; answering is the only way
	/// a message leaves the inbox.
	pub fn messages_to(&self, recipient: ClientId, limit: usize) -> Result<Vec<Message>> {
		self.select_messages(
			"m.recipient = ? AND m.status = 'open'",
			recipient.0.as_bytes(),
			"ORDER BY m.submitted_us ASC LIMIT ?",
			limit,
		)
	}

	fn select_messages(
		&self,
		condition: &str,
		client: &[u8],
		order: &str,
		limit: usize,
	) -> Result<Vec<Message>> {
		let sql = format!(
			"SELECT {COLUMNS} FROM companion_messages m
			 JOIN clients requester ON requester.id = m.requester
			 JOIN clients recipient ON recipient.id = m.recipient
			 WHERE {condition} {order}"
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(
			params![client, i64::try_from(limit).unwrap_or(i64::MAX)],
			message_from_row,
		)?;
		let mut messages = Vec::new();
		for row in rows {
			messages.push(row??);
		}
		attach_citations(&conn, &mut messages)?;
		Ok(messages)
	}

	/// Record `recipient`'s answer to `id`, with citations. Only the
	/// addressed agent may answer, only while the message is open, and every
	/// cited record must exist; anything else is [`StoreError::NotFound`],
	/// never saying which check failed.
	pub fn answer_message(
		&self,
		id: QuestionId,
		recipient: ClientId,
		answer: &str,
		citations: &[RecordId],
		now: Timestamp,
	) -> Result<()> {
		let mut citations = citations.to_vec();
		citations.sort();
		citations.dedup();
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let changed = tx.execute(
			"UPDATE companion_messages SET answer = ?3, answered_us = ?4, status = 'answered'
			 WHERE id = ?1 AND recipient = ?2 AND status = 'open'",
			params![
				id.0.as_bytes().as_slice(),
				recipient.0.as_bytes().as_slice(),
				answer,
				micros(now)
			],
		)?;
		if changed == 0 {
			return Err(StoreError::NotFound);
		}
		{
			let rowid: i64 = tx.query_row(
				"SELECT rowid FROM companion_messages WHERE id = ?1",
				[id.0.as_bytes().as_slice()],
				|row| row.get(0),
			)?;
			let mut insert = tx.prepare_cached(
				"INSERT INTO companion_citations (message, record)
				 SELECT ?1, rowid FROM records WHERE id = ?2",
			)?;
			let mut linked = 0;
			for citation in &citations {
				linked += insert.execute(params![rowid, citation.0.as_bytes().as_slice()])?;
			}
			if linked != citations.len() {
				return Err(StoreError::NotFound);
			}
		}
		tx.commit()?;
		Ok(())
	}

	/// How many companion messages `grant` submitted at or after `since`.
	pub fn message_count(&self, grant: GrantId, since: Timestamp) -> Result<u64> {
		let count: i64 = self.conn().query_row(
			"SELECT count(*) FROM companion_messages WHERE grant = ?1 AND submitted_us >= ?2",
			params![grant.0.as_bytes().as_slice(), micros(since)],
			|row| row.get(0),
		)?;
		Ok(u64::try_from(count).unwrap_or(0))
	}
}

#[cfg(test)]
mod tests {
	use quant_access::Scopes;
	use quant_access::auth::ClientKey;
	use quant_core::DeviceId;
	use quant_core::Record;
	use quant_core::TimeRange;

	use super::*;
	use crate::ClientKind;
	use crate::test_support::store;

	fn at(minutes: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_mins(minutes)
	}

	fn client(store: &Store, name: &str, kind: ClientKind) -> ClientId {
		let agent = (kind == ClientKind::Agent).then(|| {
			quant_access::AgentIdentity {
				product: name.into(),
				vendor: "Quant tests".into(),
				model: "test-model".into(),
				version: "1".into(),
				host: "test-host".into(),
				operator: None,
			}
		});
		store
			.register_client(
				name,
				kind,
				ClientKey::generate().public(),
				&Scopes::default(),
				agent.as_ref(),
			)
			.unwrap()
			.id
	}

	fn message(from: ClientId, to: ClientId, minute: i64, dedupe: &str) -> Message {
		Message {
			id: QuestionId::new(),
			requester: from,
			requester_name: "Phone".into(),
			recipient: to,
			recipient_name: "Claude".into(),
			grant: GrantId::new(),
			submitted_at: at(minute),
			question: "How did I sleep after late coffee?".into(),
			purpose: Some("the owner asked on their phone".into()),
			dedupe: dedupe.into(),
			status: MessageStatus::Open,
			answer: None,
			answered_at: None,
			citations: Vec::new(),
		}
	}

	fn record(minute: i64, text: &str) -> Record {
		Record::new("screen.frame".parse().unwrap(), DeviceId::new())
			.starting(at(minute))
			.with_text(text)
	}

	#[test]
	fn a_message_is_stored_listed_and_read_back() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let other = client(&store, "Other", ClientKind::Agent);
		let open = message(phone, agent, 0, "phone-0001");
		let stored = store.insert_message(&open).unwrap();
		assert_eq!(stored.id, open.id);
		assert_eq!(stored.status, MessageStatus::Open);
		assert_eq!(stored.requester_name, "Phone");
		assert_eq!(stored.recipient_name, "Claude");

		assert_eq!(store.messages_to(agent, 10).unwrap().len(), 1);
		assert!(store.messages_to(other, 10).unwrap().is_empty());
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);
		assert_eq!(store.message(open.id).unwrap().unwrap(), stored);
		assert_eq!(store.messages_to(agent, 10).unwrap()[0].id, open.id);
	}

	#[test]
	fn dedupe_returns_the_original_message_per_requester() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let other_phone = client(&store, "Other phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let first = message(phone, agent, 0, "phone-0001");
		let stored = store.insert_message(&first).unwrap();

		let retry = message(phone, agent, 1, "phone-0001");
		let back = store.insert_message(&retry).unwrap();
		assert_eq!(
			back.id, stored.id,
			"the retry must not create a second message"
		);
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);
		assert_eq!(store.messages_to(agent, 10).unwrap().len(), 1);

		// The same key is scoped to its requester, and other keys are
		// independent.
		let other = message(other_phone, agent, 1, "phone-0001");
		assert_eq!(store.insert_message(&other).unwrap().id, other.id);
		let different = message(phone, agent, 2, "phone-0002");
		assert_eq!(store.insert_message(&different).unwrap().id, different.id);
	}

	#[test]
	fn only_the_recipient_answers_once_and_citations_must_exist() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let other = client(&store, "Other agent", ClientKind::Agent);
		let open = message(phone, agent, 0, "phone-0001");
		store.insert_message(&open).unwrap();

		let records = store
			.insert_records(&[record(1, "slept badly"), record(2, "coffee at 21:00")])
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

		// Someone other than the recipient cannot answer.
		assert!(matches!(
			store.answer_message(open.id, other, "not mine", &[], at(3)),
			Err(StoreError::NotFound)
		));
		// A citation that names no record fails the whole answer.
		assert!(matches!(
			store.answer_message(open.id, agent, "grounded", &[RecordId::new()], at(3)),
			Err(StoreError::NotFound)
		));
		// And the message is still open, with no citations attached.
		let untouched = store.message(open.id).unwrap().unwrap();
		assert_eq!(untouched.status, MessageStatus::Open);
		assert!(untouched.answer.is_none());
		assert!(untouched.citations.is_empty());

		store
			.answer_message(open.id, agent, "Badly: 5.2 hours.", &cited, at(4))
			.unwrap();
		let answered = store.message(open.id).unwrap().unwrap();
		assert_eq!(answered.status, MessageStatus::Answered);
		assert_eq!(answered.answer.as_deref(), Some("Badly: 5.2 hours."));
		assert_eq!(answered.answered_at, Some(at(4)));
		let mut served = answered.citations.clone();
		served.sort();
		let mut expected = cited.clone();
		expected.sort();
		assert_eq!(served, expected);

		// Only once, and answering removes it from the inbox.
		assert!(matches!(
			store.answer_message(open.id, agent, "again", &[], at(5)),
			Err(StoreError::NotFound)
		));
		assert!(store.messages_to(agent, 10).unwrap().is_empty());
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);
	}

	#[test]
	fn stale_messages_are_abandoned() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let stale = message(phone, agent, 0, "phone-0001");
		let fresh = message(phone, agent, 60 * 24 * 6, "phone-0002");
		store.insert_message(&stale).unwrap();
		store.insert_message(&fresh).unwrap();

		assert_eq!(store.abandon_stale_messages(at(60 * 24 * 8)).unwrap(), 1);
		assert_eq!(
			store.message(stale.id).unwrap().unwrap().status,
			MessageStatus::Abandoned
		);
		assert_eq!(
			store.message(fresh.id).unwrap().unwrap().status,
			MessageStatus::Open
		);
		assert!(matches!(
			store.answer_message(stale.id, agent, "late", &[], at(60 * 24 * 8)),
			Err(StoreError::NotFound)
		));
	}

	#[test]
	fn counts_submissions_per_grant() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let grant = GrantId::new();
		let mut first = message(phone, agent, 0, "phone-0001");
		first.grant = grant;
		store.insert_message(&first).unwrap();
		assert_eq!(
			store
				.message_count(grant, at(0) - SignedDuration::from_hours(1))
				.unwrap(),
			1
		);
		assert_eq!(store.message_count(GrantId::new(), at(0)).unwrap(), 0);
	}

	#[test]
	fn deleting_a_cited_record_cascades_out_of_the_reply() {
		let store = store();
		let phone = client(&store, "Phone", ClientKind::Device);
		let agent = client(&store, "Claude", ClientKind::Agent);
		let open = message(phone, agent, 0, "phone-0001");
		store.insert_message(&open).unwrap();
		store.insert_records(&[record(1, "the citation")]).unwrap();
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
			.answer_message(open.id, agent, "an answer", &cited, at(2))
			.unwrap();

		store
			.purge(
				&crate::Selector::all(),
				TimeRange::new(at(0), at(10)).unwrap(),
			)
			.unwrap();
		let back = store.message(open.id).unwrap().unwrap();
		assert_eq!(back.status, MessageStatus::Answered);
		assert!(
			back.citations.is_empty(),
			"the citation went with its record"
		);
	}
}
