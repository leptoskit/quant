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
//! recipient; the first answer is stored and a simultaneous equivalent
//! request reads that same reply back. Submission is one transaction too:
//! the dedupe decision, the payload a retry must match, both sides' current
//! grants (including natural expiry), and the grant's hourly cap are settled
//! with the insert, so no check can go stale before the row exists.

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Denied;
use quant_access::Grant;
use quant_core::ClientId;
use quant_core::GrantId;
use quant_core::QuestionId;
use quant_core::RecordId;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::TransactionBehavior;
use rusqlite::params;
use uuid::Uuid;

use crate::ClientKind;
use crate::ClientStatus;
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
	/// grant was revoked, expired, or lost its companion permission.
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

/// A submission: the stored message, and whether this call created it.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
	/// The message as stored.
	pub message: Message,
	/// Whether this call inserted it. `false` means the same requester had
	/// already used the dedupe key for an identical request.
	pub created: bool,
}

/// A recorded reply: the message as stored, and whether this call wrote it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answered {
	/// The message with the reply as stored.
	pub message: Message,
	/// Whether this call recorded the reply. `false` means an equivalent
	/// answer was already stored by the request that won the race.
	pub stored: bool,
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

fn attach_citations(conn: &Connection, messages: &mut [Message]) -> Result<()> {
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

/// One message by id, with its citations, on a connection the caller
/// already holds. Used both inside a transaction and by the public reads.
fn message_at(conn: &Connection, id: QuestionId) -> Result<Option<Message>> {
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
		attach_citations(conn, std::slice::from_mut(&mut message))?;
		Ok(Some(message))
	} else {
		Ok(None)
	}
}

/// What a submission reads about an existing row before deciding whether it
/// is a retry.
struct ExistingSubmission {
	id: Vec<u8>,
	recipient: Vec<u8>,
	question: String,
	purpose: Option<String>,
}

/// What an answer reads about its row before deciding whether it may write.
struct StoredReply {
	requester: Vec<u8>,
	recipient: Vec<u8>,
	status: String,
	answer: Option<String>,
}

/// Whether `client` may still take part in a companion conversation at
/// `now`. A companion must be an approved device or app whose current grant
/// is live and still carries `companion:write`; an agent must be approved
/// with a live grant. A revoked, expired, replaced, or unrecognised client
/// is not live.
fn conversation_live(conn: &Connection, client: ClientId, now: Timestamp) -> Result<bool> {
	let Some(client) = crate::access::client_at(conn, client)? else {
		return Ok(false);
	};
	if client.status != ClientStatus::Active {
		return Ok(false);
	}
	let Some(grant) = crate::access::active_grant_at(conn, client.id)? else {
		return Ok(false);
	};
	if grant.check_live(now).is_err() {
		return Ok(false);
	}
	Ok(match client.kind {
		ClientKind::Agent => true,
		ClientKind::Device | ClientKind::App => grant.scopes.can_companion(),
		ClientKind::Cli => false,
	})
}

/// The requester's current grant, checked for a submission at `now`. A
/// request admitted under a grant the owner has since replaced is stale:
/// the caller re-reads its grant and signs again.
fn live_requester(
	conn: &Connection,
	requester: ClientId,
	grant: GrantId,
	now: Timestamp,
) -> Result<Grant> {
	let Some(client) = crate::access::client_at(conn, requester)? else {
		return Err(StoreError::Denied(Denied::Revoked));
	};
	if client.status != ClientStatus::Active
		|| !matches!(client.kind, ClientKind::Device | ClientKind::App)
	{
		return Err(StoreError::Denied(Denied::Revoked));
	}
	let Some(current) = crate::access::active_grant_at(conn, requester)? else {
		return Err(StoreError::Denied(Denied::Revoked));
	};
	if current.id != grant {
		return Err(StoreError::Denied(Denied::Revoked));
	}
	current.check_live(now)?;
	if !current.scopes.can_companion() {
		return Err(StoreError::Denied(Denied::OutOfScope));
	}
	Ok(current)
}

/// Check that the addressed agent is approved and holds a live grant.
/// Anything else is [`StoreError::NotFound`], so a companion cannot tell an
/// unknown agent from one the owner has not approved.
fn live_recipient(conn: &Connection, recipient: ClientId, now: Timestamp) -> Result<()> {
	let Some(client) = crate::access::client_at(conn, recipient)? else {
		return Err(StoreError::NotFound);
	};
	if client.kind != ClientKind::Agent || client.status != ClientStatus::Active {
		return Err(StoreError::NotFound);
	}
	let Some(grant) = crate::access::active_grant_at(conn, recipient)? else {
		return Err(StoreError::NotFound);
	};
	grant.check_live(now).map_err(|_| StoreError::NotFound)
}

impl Store {
	/// Close every open companion message that can no longer be delivered
	/// or answered: past [`MESSAGE_WINDOW`], or with either side's current
	/// grant revoked, expired, or missing its companion permission. A
	/// week-old question is better recorded as abandoned than answered
	/// late, and an expired sender must not leave a replyable question
	/// behind.
	pub fn abandon_stale_messages(&self, now: Timestamp) -> Result<u64> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let cutoff = micros(now - MESSAGE_WINDOW);
		let mut changed = tx.execute(
			"UPDATE companion_messages SET status = 'abandoned'
			 WHERE status = 'open' AND submitted_us < ?1",
			[cutoff],
		)?;
		// Only the distinct clients of still-open messages are checked, not
		// one grant read per message: the conversation sides are bounded by
		// the clients that have ever messaged.
		let clients: Vec<Vec<u8>> = {
			let mut statement = tx.prepare(
				"SELECT requester FROM companion_messages WHERE status = 'open'
				 UNION
				 SELECT recipient FROM companion_messages WHERE status = 'open'",
			)?;
			let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
			rows.collect::<rusqlite::Result<_>>()?
		};
		let mut lapsed = Vec::new();
		for bytes in clients {
			let client = ClientId(
				Uuid::from_slice(&bytes).map_err(|_| StoreError::Corrupt("message client"))?,
			);
			if !conversation_live(&tx, client, now)? {
				lapsed.push(bytes);
			}
		}
		if !lapsed.is_empty() {
			let placeholders = vec!["?"; lapsed.len()].join(", ");
			let sql = format!(
				"UPDATE companion_messages SET status = 'abandoned'
				 WHERE status = 'open'
				   AND (requester IN ({placeholders}) OR recipient IN ({placeholders}))"
			);
			let bound = lapsed.iter().chain(lapsed.iter()).cloned();
			changed += tx.execute(&sql, rusqlite::params_from_iter(bound))?;
		}
		tx.commit()?;
		Ok(u64::try_from(changed).unwrap_or(0))
	}

	/// Store `message` unless its requester already used its dedupe key.
	///
	/// The dedupe decision, the payload a retry must match, both sides'
	/// current grants (including natural expiry), and the requester grant's
	/// `questions_per_hour` cap are settled in the same write transaction as
	/// the insert. A revoked or expired side therefore either stops the
	/// insert, or its revocation closes the row this insert just wrote.
	pub fn submit_message(&self, message: &Message) -> Result<Submission> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let (id, created) = {
			let existing: Option<ExistingSubmission> = tx
				.query_row(
					"SELECT id, recipient, question, purpose
					 FROM companion_messages WHERE requester = ?1 AND dedupe = ?2",
					params![
						message.requester.0.as_bytes().as_slice(),
						message.dedupe.as_str()
					],
					|row| {
						Ok(ExistingSubmission {
							id: row.get(0)?,
							recipient: row.get(1)?,
							question: row.get(2)?,
							purpose: row.get(3)?,
						})
					},
				)
				.optional()?;
			if let Some(existing) = existing {
				// A retry is only the same request when it names the same
				// target and text. A key reused for anything else is a
				// caller mistake, never an idempotent replay.
				if existing.recipient.as_slice() != message.recipient.0.as_bytes().as_slice()
					|| existing.question != message.question
					|| existing.purpose != message.purpose
				{
					return Err(StoreError::Invalid(
						"that dedupe key already names a different message".into(),
					));
				}
				(
					QuestionId(
						Uuid::from_slice(&existing.id)
							.map_err(|_| StoreError::Corrupt("message id"))?,
					),
					false,
				)
			} else {
				let grant =
					live_requester(&tx, message.requester, message.grant, message.submitted_at)?;
				live_recipient(&tx, message.recipient, message.submitted_at)?;
				let since = micros(message.submitted_at - SignedDuration::from_hours(1));
				let recent: i64 = tx.query_row(
					"SELECT count(*) FROM companion_messages
					 WHERE grant = ?1 AND submitted_us >= ?2",
					params![message.grant.0.as_bytes().as_slice(), since],
					|row| row.get(0),
				)?;
				if u64::try_from(recent).unwrap_or(u64::MAX) >= grant.limits.questions_per_hour {
					return Err(StoreError::Denied(Denied::RateLimited));
				}
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
				(message.id, true)
			}
		};
		tx.commit()?;
		drop(conn);
		let stored = self.message(id)?.ok_or(StoreError::NotFound)?;
		Ok(Submission {
			message: stored,
			created,
		})
	}

	/// One message by id, with its citations.
	pub fn message(&self, id: QuestionId) -> Result<Option<Message>> {
		message_at(&self.conn(), id)
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
	/// a message leaves the inbox, and a message whose sides have lapsed is
	/// closed by [`Store::abandon_stale_messages`] before it can be listed.
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
	/// addressed agent may answer, only while the message is open, both
	/// sides must still hold live grants, and every cited record must exist;
	/// anything else is [`StoreError::NotFound`], never saying which check
	/// failed. An equivalent answer that lost a race with `recipient`'s own
	/// successful request reads the stored reply back instead of failing, so
	/// a retry after a timeout has one sensible result.
	pub fn answer_message(
		&self,
		id: QuestionId,
		recipient: ClientId,
		answer: &str,
		citations: &[RecordId],
		now: Timestamp,
	) -> Result<Answered> {
		let mut citations = citations.to_vec();
		citations.sort();
		citations.dedup();
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let row: Option<StoredReply> = tx
			.query_row(
				"SELECT requester, recipient, status, answer
				 FROM companion_messages WHERE id = ?1",
				[id.0.as_bytes().as_slice()],
				|row| {
					Ok(StoredReply {
						requester: row.get(0)?,
						recipient: row.get(1)?,
						status: row.get(2)?,
						answer: row.get(3)?,
					})
				},
			)
			.optional()?;
		let Some(row) = row else {
			return Err(StoreError::NotFound);
		};
		if row.recipient.as_slice() != recipient.0.as_bytes().as_slice() {
			return Err(StoreError::NotFound);
		}
		match MessageStatus::parse(&row.status)? {
			MessageStatus::Answered => {
				if row.answer.as_deref() != Some(answer) {
					return Err(StoreError::NotFound);
				}
				let mut stored = message_at(&tx, id)?.ok_or(StoreError::NotFound)?;
				stored.citations.sort();
				stored.citations.dedup();
				if stored.citations != citations {
					return Err(StoreError::NotFound);
				}
				tx.commit()?;
				return Ok(Answered {
					message: stored,
					stored: false,
				});
			}
			MessageStatus::Abandoned => return Err(StoreError::NotFound),
			MessageStatus::Open => {}
		}
		// A grant that lapsed after the question was left open makes it
		// unanswerable even if a listing has not swept the message yet.
		let requester = ClientId(
			Uuid::from_slice(&row.requester)
				.map_err(|_| StoreError::Corrupt("message requester"))?,
		);
		if !conversation_live(&tx, requester, now)? || !conversation_live(&tx, recipient, now)? {
			return Err(StoreError::NotFound);
		}
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
		let stored = message_at(&tx, id)?.ok_or(StoreError::NotFound)?;
		tx.commit()?;
		Ok(Answered {
			message: stored,
			stored: true,
		})
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
	use std::sync::Arc;
	use std::sync::Barrier;

	use quant_access::Scopes;
	use quant_access::auth::ClientKey;
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

	/// Register a client and approve `scopes` for it, as the owner would.
	fn approved(
		store: &Store,
		name: &str,
		kind: ClientKind,
		scopes: &[&str],
	) -> (ClientId, GrantId) {
		let client = client(store, name, kind);
		let grant = Grant::new(client, name, Scopes::parse(scopes).unwrap());
		store.approve(&grant).unwrap();
		(client, grant.id)
	}

	fn message(from: ClientId, grant: GrantId, to: ClientId, minute: i64, dedupe: &str) -> Message {
		Message {
			id: QuestionId::new(),
			requester: from,
			requester_name: "Phone".into(),
			recipient: to,
			recipient_name: "Claude".into(),
			grant,
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

	/// Make the client's current grant expire at `expiry`, without revoking
	/// or replacing it: the natural expiry a clock reaches on its own.
	fn lapse(store: &Store, client: ClientId, expiry: Timestamp) {
		let mut grant = store.active_grant(client).unwrap().unwrap();
		grant.expires_at = Some(expiry);
		store.update_grant(&grant).unwrap();
	}

	fn record(minute: i64, text: &str) -> Record {
		Record::new("screen.frame".parse().unwrap(), DeviceId::new())
			.starting(at(minute))
			.with_text(text)
	}

	#[test]
	fn a_message_is_stored_listed_and_read_back() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let (other, _) = approved(&store, "Other", ClientKind::Agent, &["screen:read"]);
		let open = message(phone, grant, agent, 0, "phone-0001");
		let submission = store.submit_message(&open).unwrap();
		assert!(submission.created);
		let stored = submission.message;
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
		let (phone, phone_grant) =
			approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (other_phone, other_grant) = approved(
			&store,
			"Other phone",
			ClientKind::Device,
			&["companion:write"],
		);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let first = message(phone, phone_grant, agent, 0, "phone-0001");
		let stored = store.submit_message(&first).unwrap();
		assert!(stored.created);

		let retry = message(phone, phone_grant, agent, 1, "phone-0001");
		let back = store.submit_message(&retry).unwrap();
		assert!(!back.created);
		assert_eq!(
			back.message.id, stored.message.id,
			"the retry must not create a second message"
		);
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);
		assert_eq!(store.messages_to(agent, 10).unwrap().len(), 1);

		// The same key with different text is a caller mistake, refused
		// inside the same transaction rather than replayed as the old id.
		let mut different_text = retry.clone();
		different_text.question = "A different question".into();
		assert!(matches!(
			store.submit_message(&different_text),
			Err(StoreError::Invalid(_))
		));
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);

		// The same key is scoped to its requester, and other keys are
		// independent.
		let other = message(other_phone, other_grant, agent, 1, "phone-0001");
		assert!(store.submit_message(&other).unwrap().created);
		let different = message(phone, phone_grant, agent, 2, "phone-0002");
		assert!(store.submit_message(&different).unwrap().created);
	}

	#[test]
	fn only_the_recipient_answers_once_and_citations_must_exist() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let (other, _) = approved(&store, "Other agent", ClientKind::Agent, &["screen:read"]);
		let open = message(phone, grant, agent, 0, "phone-0001");
		store.submit_message(&open).unwrap();

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

		let first = store
			.answer_message(open.id, agent, "Badly: 5.2 hours.", &cited, at(4))
			.unwrap();
		assert!(first.stored);
		let answered = first.message;
		assert_eq!(answered.status, MessageStatus::Answered);
		assert_eq!(answered.answer.as_deref(), Some("Badly: 5.2 hours."));
		assert_eq!(answered.answered_at, Some(at(4)));
		let mut served = answered.citations.clone();
		served.sort();
		let mut expected = cited.clone();
		expected.sort();
		assert_eq!(served, expected);

		// The equivalent request that lost the race reads the same stored
		// reply back instead of failing.
		let replay = store
			.answer_message(open.id, agent, "Badly: 5.2 hours.", &cited, at(5))
			.unwrap();
		assert!(!replay.stored);
		assert_eq!(replay.message, answered);

		// A different answer, or the same text with different citations,
		// is not a retry.
		assert!(matches!(
			store.answer_message(open.id, agent, "again", &[], at(5)),
			Err(StoreError::NotFound)
		));
		assert!(matches!(
			store.answer_message(open.id, agent, "Badly: 5.2 hours.", &[], at(5)),
			Err(StoreError::NotFound)
		));
		assert!(store.messages_to(agent, 10).unwrap().is_empty());
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);
	}

	#[test]
	fn stale_messages_are_abandoned() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let stale = message(phone, grant, agent, 0, "phone-0001");
		let fresh = message(phone, grant, agent, 60 * 24 * 6, "phone-0002");
		store.submit_message(&stale).unwrap();
		store.submit_message(&fresh).unwrap();

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
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let first = message(phone, grant, agent, 0, "phone-0001");
		store.submit_message(&first).unwrap();
		assert_eq!(
			store
				.message_count(grant, at(0) - SignedDuration::from_hours(1))
				.unwrap(),
			1
		);
		assert_eq!(store.message_count(GrantId::new(), at(0)).unwrap(), 0);
	}

	#[test]
	fn a_lapsed_sender_cannot_submit_or_leave_a_message_answerable() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = store
			.submit_message(&message(phone, grant, agent, 0, "phone-0001"))
			.unwrap()
			.message;

		lapse(&store, phone, at(5));

		// A new submission under the expired grant is refused without a row.
		assert!(matches!(
			store.submit_message(&message(phone, grant, agent, 10, "phone-0002")),
			Err(StoreError::Denied(Denied::Expired))
		));
		assert_eq!(store.messages_from(phone, 10).unwrap().len(), 1);

		// The question left open is no longer answerable, and the next read
		// records it as abandoned rather than delivering it.
		assert!(matches!(
			store.answer_message(open.id, agent, "late", &[], at(6)),
			Err(StoreError::NotFound)
		));
		assert_eq!(
			store.message(open.id).unwrap().unwrap().status,
			MessageStatus::Open,
			"the refusal is a liveness check, not a quiet answer"
		);
		assert_eq!(store.abandon_stale_messages(at(6)).unwrap(), 1);
		assert_eq!(
			store.message(open.id).unwrap().unwrap().status,
			MessageStatus::Abandoned
		);
		assert!(store.messages_to(agent, 10).unwrap().is_empty());
	}

	#[test]
	fn a_lapsed_recipient_cannot_receive_or_keep_messages() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = store
			.submit_message(&message(phone, grant, agent, 0, "phone-0001"))
			.unwrap()
			.message;

		lapse(&store, agent, at(5));

		assert!(matches!(
			store.submit_message(&message(phone, grant, agent, 10, "phone-0002")),
			Err(StoreError::NotFound)
		));
		assert!(matches!(
			store.answer_message(open.id, agent, "late", &[], at(6)),
			Err(StoreError::NotFound)
		));
		assert_eq!(store.abandon_stale_messages(at(6)).unwrap(), 1);
		assert_eq!(
			store.message(open.id).unwrap().unwrap().status,
			MessageStatus::Abandoned
		);
		assert!(store.messages_to(agent, 10).unwrap().is_empty());
	}

	#[test]
	fn revoking_either_side_stops_new_messages_and_closes_open_ones() {
		// Revoking the sender's grant stops submissions and closes what it
		// left waiting, even though the message still names that grant.
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = store
			.submit_message(&message(phone, grant, agent, 0, "phone-0001"))
			.unwrap()
			.message;
		store.revoke_grant(grant).unwrap();
		assert!(matches!(
			store.submit_message(&message(phone, grant, agent, 5, "phone-0002")),
			Err(StoreError::Denied(Denied::Revoked))
		));
		assert_eq!(
			store.message(open.id).unwrap().unwrap().status,
			MessageStatus::Abandoned
		);
		assert!(store.messages_to(agent, 10).unwrap().is_empty());

		// Revoking the recipient's grant closes the conversation the same
		// way, while a retry of the requester's own earlier send still reads
		// that message back.
		let other = crate::test_support::store();
		let (phone, grant) = approved(&other, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, agent_grant) = approved(&other, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = other
			.submit_message(&message(phone, grant, agent, 0, "phone-0001"))
			.unwrap()
			.message;
		other.revoke_grant(agent_grant).unwrap();
		assert!(matches!(
			other.submit_message(&message(phone, grant, agent, 5, "phone-0002")),
			Err(StoreError::NotFound)
		));
		assert!(matches!(
			other.answer_message(open.id, agent, "late", &[], at(5)),
			Err(StoreError::NotFound)
		));
		let replay = other
			.submit_message(&message(phone, grant, agent, 6, "phone-0001"))
			.unwrap();
		assert!(!replay.created);
		assert_eq!(replay.message.id, open.id);
		assert_eq!(replay.message.status, MessageStatus::Abandoned);
	}

	#[test]
	fn the_hourly_cap_is_settled_with_the_insert() {
		let store = store();
		let (phone, _) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let mut grant = store.active_grant(phone).unwrap().unwrap();
		grant.limits.questions_per_hour = 2;
		store.update_grant(&grant).unwrap();
		let grant_id = grant.id;

		let first = store
			.submit_message(&message(phone, grant_id, agent, 0, "phone-0001"))
			.unwrap();
		assert!(first.created);
		assert!(
			store
				.submit_message(&message(phone, grant_id, agent, 1, "phone-0002"))
				.unwrap()
				.created
		);
		// The third submission is refused and cannot slip a row in beside
		// the count that saw room.
		assert!(matches!(
			store.submit_message(&message(phone, grant_id, agent, 2, "phone-0003")),
			Err(StoreError::Denied(Denied::RateLimited))
		));
		assert_eq!(
			store
				.message_count(grant_id, at(0) - SignedDuration::from_hours(1))
				.unwrap(),
			2
		);
		// A retry of the first is not a new submission and still returns it.
		let retried = store
			.submit_message(&message(phone, grant_id, agent, 2, "phone-0001"))
			.unwrap();
		assert!(!retried.created);
		assert_eq!(retried.message.id, first.message.id);
	}

	#[test]
	fn simultaneous_equivalent_answers_store_one_reply() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("quant.db");
		let key = quant_crypto::VaultKey::generate();
		let first = Store::open(&path, &key).unwrap();
		let second = Store::open(&path, &key).unwrap();
		let (phone, grant) = approved(&first, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&first, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = first
			.submit_message(&message(phone, grant, agent, 0, "phone-0001"))
			.unwrap()
			.message;

		let barrier = Arc::new(Barrier::new(2));
		let worker = |store: Store, barrier: Arc<Barrier>| {
			std::thread::spawn(move || {
				barrier.wait();
				store
					.answer_message(open.id, agent, "Badly: 5.2 hours.", &[], at(3))
					.map(|answered| (answered.stored, answered.message))
					.map_err(|error| error.to_string())
			})
		};
		let left = worker(first, Arc::clone(&barrier));
		let right = worker(second, barrier);
		let left = left.join().unwrap().unwrap();
		let right = right.join().unwrap().unwrap();

		assert_ne!(
			left.0, right.0,
			"exactly one simultaneous request records the reply"
		);
		assert_eq!(left.1, right.1, "both requests see the same stored reply");
		assert_eq!(left.1.status, MessageStatus::Answered);
		assert_eq!(left.1.answer.as_deref(), Some("Badly: 5.2 hours."));
	}

	#[test]
	fn simultaneous_retries_with_one_key_cannot_both_insert() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("quant.db");
		let key = quant_crypto::VaultKey::generate();
		let first = Store::open(&path, &key).unwrap();
		let second = Store::open(&path, &key).unwrap();
		let (phone, grant) = approved(&first, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&first, "Claude", ClientKind::Agent, &["screen:read"]);
		let left = message(phone, grant, agent, 0, "phone-0001");
		let mut right = message(phone, grant, agent, 0, "phone-0001");
		right.question = "A different question".into();

		let barrier = Arc::new(Barrier::new(2));
		let worker = |store: Store, message: Message, barrier: Arc<Barrier>| {
			std::thread::spawn(move || {
				barrier.wait();
				store
					.submit_message(&message)
					.map(|submission| (submission.message.id, submission.created))
					.map_err(|error| error.to_string())
			})
		};
		let first_result = worker(first, left, Arc::clone(&barrier));
		let second_result = worker(second, right, barrier);
		let results = [first_result.join().unwrap(), second_result.join().unwrap()];

		assert_eq!(
			results.iter().filter(|result| result.is_ok()).count(),
			1,
			"only the request that won the race may insert"
		);
		assert!(
			results
				.iter()
				.any(|result| matches!(result, Err(error) if error.contains("dedupe"))),
			"the loser is refused because its payload does not match"
		);
		let check = Store::open(&path, &key).unwrap();
		assert_eq!(check.messages_from(phone, 10).unwrap().len(), 1);
	}

	#[test]
	fn deleting_a_cited_record_cascades_out_of_the_reply() {
		let store = store();
		let (phone, grant) = approved(&store, "Phone", ClientKind::Device, &["companion:write"]);
		let (agent, _) = approved(&store, "Claude", ClientKind::Agent, &["screen:read"]);
		let open = message(phone, grant, agent, 0, "phone-0001");
		store.submit_message(&open).unwrap();
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
