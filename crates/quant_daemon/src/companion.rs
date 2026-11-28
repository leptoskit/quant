//! Companion messaging: one separately approved companion client asks one
//! approved agent a question, and later reads the agent's actual reply.
//!
//! This is deliberately not the agent-opened questions ledger. A companion
//! message never appears in `/v1/questions/mine`, and an agent's own
//! questions never appear in the inbox. The agent picks messages up with
//! `GET /v1/inbox`; nothing is pushed, and listing marks nothing read. Only
//! answering a message (`POST /v1/inbox/{id}/answer`) records a reply, so a
//! caller can always tell an actual persisted answer from an agent that never
//! ran.
//!
//! A companion must be paired as a device or app and hold the explicit
//! `companion:write` scope. Read and write scopes never imply it, and an
//! agent cannot use these routes even if it somehow holds the scope. The
//! recipient must be an approved agent with a live grant, and can only be
//! pulled by that agent; the requester can only read back its own messages.
//! Every input is sanitised and bounded, submissions are rate limited by the
//! grant's `questions_per_hour`, and a required dedupe key makes a retry
//! after a lost response return the original message. The retry, both sides'
//! current grants, and the cap are decided in the store's write transaction,
//! so a grant revoked or expired while the request was in flight cannot leave
//! a deliverable message behind.

use std::sync::Arc;

use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State as AxumState;
use jiff::Timestamp;
use quant_access::Denied;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_access::redact;
use quant_api::CompanionAgent;
use quant_api::CompanionAnswer;
use quant_api::CompanionMessageStatus;
use quant_api::CompanionMessageView;
use quant_api::CompanionSend;
use quant_core::QuestionId;
use quant_store::Client;
use quant_store::ClientKind;
use quant_store::ClientStatus;
use quant_store::Message;
use quant_store::MessageStatus;
use quant_store::StoreError;
use serde::Deserialize;

use crate::authn::Caller;
use crate::authn::Signed;
use crate::error::ApiResult;
use crate::error::Error;
use crate::query::audit_meta;
use crate::query::deny;
use crate::questions::MAX_ANSWER;
use crate::questions::MAX_CITATIONS;
use crate::questions::MAX_QUESTION;
use crate::questions::citation_readable;
use crate::questions::clean;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// Shortest idempotency key a companion may send.
pub const MIN_DEDUPE: usize = 8;
/// Longest idempotency key a companion may send.
pub const MAX_DEDUPE: usize = 64;
/// Default and maximum sizes for a message list.
pub const DEFAULT_LIST: usize = 50;
/// Most messages one pull or history read returns.
pub const MAX_LIST: usize = 200;

/// A list request's query string.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct ListQuery {
	/// Most messages to return (default 50, at most 200).
	#[serde(default)]
	pub limit: Option<usize>,
}

impl ListQuery {
	fn limit(self) -> usize {
		self.limit.unwrap_or(DEFAULT_LIST).clamp(1, MAX_LIST)
	}
}

/// A companion sender: a device or app, explicitly permitted to message
/// agents. Agents, the CLI, and any grant without `companion:write` are
/// refused, however many read or write scopes they hold.
fn companion_caller(caller: &Caller) -> Result<(), Error> {
	let kind = matches!(caller.client.kind, ClientKind::Device | ClientKind::App);
	if kind && caller.grant.scopes.can_companion() {
		Ok(())
	} else {
		Err(Error::Forbidden(Denied::OutOfScope))
	}
}

/// An agent caller: only the addressed agent may pull or answer messages.
fn agent_caller(caller: &Caller) -> Result<(), Error> {
	if caller.client.kind == ClientKind::Agent {
		Ok(())
	} else {
		Err(Error::Forbidden(Denied::OutOfScope))
	}
}

fn status_out(status: MessageStatus) -> CompanionMessageStatus {
	match status {
		MessageStatus::Open => CompanionMessageStatus::Open,
		MessageStatus::Answered => CompanionMessageStatus::Answered,
		MessageStatus::Abandoned => CompanionMessageStatus::Abandoned,
	}
}

/// A message shaped for serving: sanitised, and redacted unless the reader's
/// grant is unredacted.
fn shape(message: &Message, redact_text: bool) -> CompanionMessageView {
	CompanionMessageView {
		id: message.id,
		from: message.requester,
		from_name: redact::prepare(&message.requester_name, redact_text),
		to: message.recipient,
		to_name: redact::prepare(&message.recipient_name, redact_text),
		submitted_at: message.submitted_at,
		question: redact::prepare(&message.question, redact_text),
		purpose: message
			.purpose
			.as_deref()
			.map(|purpose| redact::prepare(purpose, redact_text)),
		status: status_out(message.status),
		answer: message
			.answer
			.as_deref()
			.map(|answer| redact::prepare(answer, redact_text)),
		answered_at: message.answered_at,
		citations: message.citations.clone(),
	}
}

fn agent_view(client: Client) -> CompanionAgent {
	CompanionAgent {
		id: client.id,
		name: redact::sanitize(&client.name).into_owned(),
		agent: client.agent.map(|agent| agent.sanitised()),
	}
}

fn audit(
	state: &State,
	caller: &Caller,
	action: &str,
	target: impl Into<String>,
	outcome: Outcome,
	detail: serde_json::Value,
) -> ApiResult<()> {
	audit_with_purpose(state, caller, action, target, outcome, None, detail)
}

/// Audit one companion action, preferring an explicit body purpose over the
/// signed header's purpose when the request carries one.
fn audit_with_purpose(
	state: &State,
	caller: &Caller,
	action: &str,
	target: impl Into<String>,
	outcome: Outcome,
	purpose: Option<&str>,
	detail: serde_json::Value,
) -> ApiResult<()> {
	let (header_purpose, _, model) = audit_meta(&caller.meta);
	let purpose = purpose.or(header_purpose.as_deref());
	state.vault.store.audit(
		AuditEvent::new(caller.actor(), action, target, outcome)
			.meta(purpose, None, model.as_deref())
			.detail(detail),
	)?;
	Ok(())
}

fn allowed() -> Outcome {
	Outcome::Allowed {
		records: 0,
		bytes: 0,
	}
}

/// An idempotency key: 8 to 64 ASCII characters from a deliberately small
/// alphabet, so it cannot carry hidden instructions into the vault.
fn dedupe_key(raw: &str) -> Option<String> {
	let key = raw.trim();
	if !(MIN_DEDUPE..=MAX_DEDUPE).contains(&key.len()) {
		return None;
	}
	key.chars()
		.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
		.then(|| key.to_owned())
}

/// Whether the addressed agent still holds a live grant and may be chosen.
/// Store failures propagate rather than looking like an absent agent.
fn target_live(state: &State, client: &Client, now: Timestamp) -> ApiResult<bool> {
	if client.kind != ClientKind::Agent || client.status != ClientStatus::Active {
		return Ok(false);
	}
	let Some(grant) = state.vault.store.active_grant(client.id)? else {
		return Ok(false);
	};
	Ok(grant.check_live(now).is_ok())
}

/// `GET /v1/companion/agents`: the approved agents this companion may address.
pub async fn agents(
	AxumState(state): Shared,
	request: Signed<()>,
) -> ApiResult<Json<Vec<CompanionAgent>>> {
	let caller = &request.caller;
	companion_caller(caller)
		.map_err(|error| deny(&state, caller, "companion.agents", "", error))?;
	let now = Timestamp::now();
	let mut agents = Vec::new();
	for client in state.vault.store.clients()? {
		if client.id != caller.client.id && target_live(&state, &client, now)? {
			agents.push(agent_view(client));
		}
	}
	audit(
		&state,
		caller,
		"companion.agents",
		"",
		allowed(),
		serde_json::json!({ "agents": agents.len() }),
	)?;
	Ok(Json(agents))
}

/// `POST /v1/companion/messages`: ask one approved agent a question.
pub async fn send(
	AxumState(state): Shared,
	request: Signed<CompanionSend>,
) -> ApiResult<Json<CompanionMessageView>> {
	let caller = &request.caller;
	companion_caller(caller).map_err(|error| deny(&state, caller, "companion.send", "", error))?;
	let body = request.body;
	if body.to == caller.client.id {
		return Err(Error::Invalid(
			"a companion cannot address itself; choose an approved agent".into(),
		));
	}
	let question = clean(&body.question, MAX_QUESTION);
	if question.is_empty() {
		return Err(Error::Invalid("a message needs text".into()));
	}
	let purpose = match body.purpose.as_deref() {
		Some(purpose) => {
			let purpose = clean(purpose, 500);
			if !crate::authn::PURPOSE_RANGE.contains(&purpose.chars().count()) {
				return Err(Error::Invalid("the purpose is 8 to 500 characters".into()));
			}
			Some(purpose)
		}
		None => None,
	};
	let Some(dedupe) = dedupe_key(&body.dedupe) else {
		return Err(Error::Invalid(
			"the dedupe key is 8 to 64 characters of letters, digits, `.`, `-`, `_`, or `:`".into(),
		));
	};
	let now = Timestamp::now();
	state.vault.store.abandon_stale_messages(now)?;

	// The store settles the dedupe decision, the payload a retry must
	// match, both sides' current grants, and the hourly cap in one
	// transaction with the insert. The recipient lookup here only supplies
	// the name kept with the message; the store decides eligibility again.
	let Some(target) = state.vault.store.client(body.to)? else {
		audit_with_purpose(
			&state,
			caller,
			"companion.send",
			body.to.to_string(),
			Outcome::Failed,
			purpose.as_deref(),
			serde_json::json!({ "reason": "unknown recipient" }),
		)?;
		return Err(Error::NotFound);
	};

	let message = Message {
		id: QuestionId::new(),
		requester: caller.client.id,
		requester_name: caller.client.name.clone(),
		recipient: target.id,
		recipient_name: target.name.clone(),
		grant: caller.grant.id,
		submitted_at: now,
		question: question.clone(),
		purpose: purpose.clone(),
		dedupe,
		status: MessageStatus::Open,
		answer: None,
		answered_at: None,
		citations: Vec::new(),
	};
	let submission = match state.vault.store.submit_message(&message) {
		Ok(submission) => submission,
		Err(error) => {
			let (outcome, reason) = match &error {
				StoreError::NotFound => {
					(
						Outcome::Failed,
						"recipient is not an approved agent".to_owned(),
					)
				}
				StoreError::Denied(denied) => {
					(Outcome::Denied { reason: *denied }, denied.to_string())
				}
				StoreError::Invalid(_) => {
					(
						Outcome::Failed,
						"the dedupe key names a different message".to_owned(),
					)
				}
				_ => (Outcome::Failed, "message not stored".to_owned()),
			};
			audit_with_purpose(
				&state,
				caller,
				"companion.send",
				body.to.to_string(),
				outcome,
				purpose.as_deref(),
				serde_json::json!({ "reason": reason }),
			)?;
			return Err(error.into());
		}
	};
	let stored = submission.message;
	audit_with_purpose(
		&state,
		caller,
		"companion.send",
		stored.id.to_string(),
		allowed(),
		purpose.as_deref(),
		serde_json::json!({
			"retry": !submission.created,
			"from": caller.client.id,
			"to": stored.recipient,
			"question_chars": question.chars().count(),
		}),
	)?;
	Ok(Json(shape(&stored, !caller.grant.unredacted)))
}

/// `GET /v1/companion/messages`: the requester's own submissions, newest
/// first, with their actual replies.
pub async fn sent(
	AxumState(state): Shared,
	Query(query): Query<ListQuery>,
	request: Signed<()>,
) -> ApiResult<Json<Vec<CompanionMessageView>>> {
	let caller = &request.caller;
	companion_caller(caller).map_err(|error| deny(&state, caller, "companion.sent", "", error))?;
	state.vault.store.abandon_stale_messages(Timestamp::now())?;
	let messages = state
		.vault
		.store
		.messages_from(caller.client.id, query.limit())?;
	audit(
		&state,
		caller,
		"companion.sent",
		"",
		allowed(),
		serde_json::json!({ "count": messages.len() }),
	)?;
	Ok(Json(
		messages
			.iter()
			.map(|message| shape(message, !caller.grant.unredacted))
			.collect(),
	))
}

/// `GET /v1/companion/messages/{id}`: one of the requester's own messages and
/// its actual reply.
pub async fn message(
	AxumState(state): Shared,
	Path(id): Path<String>,
	request: Signed<()>,
) -> ApiResult<Json<CompanionMessageView>> {
	let caller = &request.caller;
	companion_caller(caller).map_err(|error| deny(&state, caller, "companion.read", "", error))?;
	let id: QuestionId = id
		.parse()
		.map_err(|_| Error::Invalid("bad message id".into()))?;
	state.vault.store.abandon_stale_messages(Timestamp::now())?;
	let Some(message) = state.vault.store.message(id)? else {
		return Err(Error::NotFound);
	};
	// Only the requester's own history. The addressed agent uses its inbox,
	// never this route, and a different companion learns nothing.
	if message.requester != caller.client.id {
		audit(
			&state,
			caller,
			"companion.read",
			id.to_string(),
			Outcome::Failed,
			serde_json::json!({ "reason": "not this requester's message" }),
		)?;
		return Err(Error::NotFound);
	}
	audit(
		&state,
		caller,
		"companion.read",
		id.to_string(),
		allowed(),
		serde_json::json!({ "status": status_out(message.status) }),
	)?;
	Ok(Json(shape(&message, !caller.grant.unredacted)))
}

/// `GET /v1/inbox`: open messages addressed to this agent, oldest first.
/// Listing is a pull and changes nothing: a message stays open until the
/// agent records an actual answer.
pub async fn inbox(
	AxumState(state): Shared,
	Query(query): Query<ListQuery>,
	request: Signed<()>,
) -> ApiResult<Json<Vec<CompanionMessageView>>> {
	let caller = &request.caller;
	agent_caller(caller).map_err(|error| deny(&state, caller, "companion.inbox", "", error))?;
	state.vault.store.abandon_stale_messages(Timestamp::now())?;
	let messages = state
		.vault
		.store
		.messages_to(caller.client.id, query.limit())?;
	audit(
		&state,
		caller,
		"companion.inbox",
		"",
		allowed(),
		serde_json::json!({ "count": messages.len() }),
	)?;
	Ok(Json(
		messages
			.iter()
			.map(|message| shape(message, !caller.grant.unredacted))
			.collect(),
	))
}

/// `POST /v1/inbox/{id}/answer`: record the addressed agent's actual reply.
pub async fn answer(
	AxumState(state): Shared,
	Path(id): Path<String>,
	request: Signed<CompanionAnswer>,
) -> ApiResult<Json<CompanionMessageView>> {
	let caller = &request.caller;
	agent_caller(caller).map_err(|error| deny(&state, caller, "companion.answer", "", error))?;
	let id: QuestionId = id
		.parse()
		.map_err(|_| Error::Invalid("bad message id".into()))?;
	let body = request.body;
	let answer = clean(&body.answer, MAX_ANSWER);
	if answer.is_empty() {
		return Err(Error::Invalid("an answer needs text".into()));
	}
	if body.citations.len() > MAX_CITATIONS {
		return Err(Error::Invalid(format!(
			"an answer cites at most {MAX_CITATIONS} records"
		)));
	}
	let now = Timestamp::now();
	state.vault.store.abandon_stale_messages(now)?;
	let Some(message) = state.vault.store.message(id)? else {
		return Err(Error::NotFound);
	};
	if message.recipient != caller.client.id {
		audit(
			&state,
			caller,
			"companion.answer",
			id.to_string(),
			Outcome::Failed,
			serde_json::json!({ "reason": "not addressed to this agent" }),
		)?;
		return Err(Error::NotFound);
	}

	let mut sent = body.citations.clone();
	sent.sort();
	sent.dedup();
	match message.status {
		// A retry of the exact answer that was stored succeeds and returns
		// the stored reply. Different text on an answered message is not a
		// retry, and is refused like a second answer.
		MessageStatus::Answered => {
			let mut stored = message.citations.clone();
			stored.sort();
			stored.dedup();
			if message.answer.as_deref() == Some(answer.as_str()) && stored == sent {
				audit(
					&state,
					caller,
					"companion.answer",
					id.to_string(),
					allowed(),
					serde_json::json!({ "retry": true }),
				)?;
				return Ok(Json(shape(&message, !caller.grant.unredacted)));
			}
			audit(
				&state,
				caller,
				"companion.answer",
				id.to_string(),
				Outcome::Failed,
				serde_json::json!({ "reason": "already answered" }),
			)?;
			return Err(Error::NotFound);
		}
		MessageStatus::Abandoned => {
			audit(
				&state,
				caller,
				"companion.answer",
				id.to_string(),
				Outcome::Failed,
				serde_json::json!({ "reason": "abandoned" }),
			)?;
			return Err(Error::NotFound);
		}
		MessageStatus::Open => {}
	}

	// Every citation must name a record this agent's grant can read, or the
	// whole reply fails as not found, exactly like an answer to its own
	// question.
	if !body
		.citations
		.iter()
		.all(|record| citation_readable(&state, caller, *record))
	{
		audit(
			&state,
			caller,
			"companion.answer",
			id.to_string(),
			Outcome::Failed,
			serde_json::json!({ "reason": "citation not readable" }),
		)?;
		return Err(Error::NotFound);
	}
	let answered =
		state
			.vault
			.store
			.answer_message(id, caller.client.id, &answer, &body.citations, now)?;
	audit(
		&state,
		caller,
		"companion.answer",
		id.to_string(),
		allowed(),
		serde_json::json!({
			"retry": !answered.stored,
			"citations": body.citations.len(),
			"answer_chars": answer.chars().count(),
		}),
	)?;
	Ok(Json(shape(&answered.message, !caller.grant.unredacted)))
}
