//! The questions routes: what agents asked, what they answered, and the
//! trail the owner can read.
//!
//! Every question carries the purpose it was opened with, the model that
//! ran it, and the totals of what was served under it. Answers are text
//! about the owner, so they leave the vault only for the owner and for the
//! client that wrote them — never for another agent.

use std::sync::Arc;

use axum::Json;
use axum::extract::Path;
use axum::extract::State as AxumState;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_access::redact;
use quant_api::QuestionOpen;
use quant_api::QuestionStatus as ApiStatus;
use quant_core::QuestionId;
use quant_store::Question;
use quant_store::QuestionStatus;

use crate::authn::Owner;
use crate::authn::Signed;
use crate::error::ApiResult;
use crate::error::Error;
use crate::query::audit_meta;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// How long a question may be, in characters.
pub(crate) const MAX_QUESTION: usize = 2_000;
/// How long an answer may be, in characters.
pub(crate) const MAX_ANSWER: usize = 8_000;
/// Most citations one answer may carry.
pub(crate) const MAX_CITATIONS: usize = 50;

pub(crate) fn clean(text: &str, cap: usize) -> String {
	redact::sanitize(text).trim().chars().take(cap).collect()
}

/// A stored question shaped for serving: sanitised, and redacted when the
/// reader is the agent that asked (the owner reads it unredacted).
fn shape(question: &Question, redact_text: bool) -> quant_api::QuestionView {
	quant_api::QuestionView {
		id: question.id,
		client: question.client,
		grant: question.grant,
		asked_at: question.asked_at,
		question: redact::prepare(&question.question, redact_text),
		purpose: redact::prepare(&question.purpose, redact_text),
		model: question.model.clone(),
		status: match question.status {
			QuestionStatus::Open => ApiStatus::Open,
			QuestionStatus::Answered => ApiStatus::Answered,
			QuestionStatus::Abandoned => ApiStatus::Abandoned,
		},
		answer: question
			.answer
			.as_ref()
			.map(|answer| redact::prepare(answer, redact_text)),
		answered_at: question.answered_at,
		citations: question.citations.clone(),
		records: question.records,
		bytes: question.bytes,
	}
}

/// `POST /v1/questions`: open a question and get its id.
pub async fn open(
	AxumState(state): Shared,
	request: Signed<QuestionOpen>,
) -> ApiResult<Json<quant_api::QuestionOpened>> {
	let caller = &request.caller;
	let body = request.body;
	let question_text = clean(&body.question, MAX_QUESTION);
	let purpose = clean(&body.purpose, 500);
	if question_text.is_empty() {
		return Err(Error::Invalid("a question needs text".into()));
	}
	if !crate::authn::PURPOSE_RANGE.contains(&purpose.chars().count()) {
		return Err(Error::Invalid("the purpose is 8 to 500 characters".into()));
	}
	let model = body
		.model
		.as_deref()
		.map(|m| clean(m, 120))
		.filter(|m| !m.is_empty());
	let now = Timestamp::now();
	// At most the grant's questions per hour.
	state.vault.store.abandon_stale_questions(now)?;
	let hour_ago = now - SignedDuration::from_hours(1);
	let recent = state
		.vault
		.store
		.question_count(caller.grant.id, hour_ago)?;
	if recent >= caller.grant.limits.questions_per_hour {
		let (purpose, question, model) = audit_meta(&caller.meta);
		let _ = state.vault.store.audit(
			AuditEvent::new(
				caller.actor(),
				"question.open",
				"",
				Outcome::Denied {
					reason: quant_access::Denied::RateLimited,
				},
			)
			.meta(purpose.as_deref(), question, model.as_deref()),
		);
		return Err(Error::Forbidden(quant_access::Denied::RateLimited));
	}
	let question = Question {
		id: QuestionId::new(),
		client: caller.client.id,
		grant: caller.grant.id,
		asked_at: now,
		question: question_text.clone(),
		purpose: purpose.clone(),
		model: model.clone(),
		status: QuestionStatus::Open,
		answer: None,
		answered_at: None,
		citations: Vec::new(),
		records: 0,
		bytes: 0,
	};
	state.vault.store.insert_question(&question)?;
	state.vault.store.audit(
		AuditEvent::new(
			caller.actor(),
			"question.open",
			question.id.to_string(),
			Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.meta(Some(&purpose), Some(question.id), model.as_deref())
		.detail(serde_json::json!({ "question_chars": question_text.chars().count() })),
	)?;
	Ok(Json(quant_api::QuestionOpened { id: question.id }))
}

/// Whether the grant could read the record `id` at read level: the record
/// exists, its stream is in scope at read level or above, and its classes
/// are not denied. Unreadable ids make the whole answer fail as not found,
/// never saying which.
pub(crate) fn citation_readable(
	state: &State,
	caller: &crate::authn::Caller,
	id: quant_core::RecordId,
) -> bool {
	let Ok(Some(record)) = state.vault.store.record(id) else {
		return false;
	};
	caller.grant.scopes.read_level(&record.stream) >= Some(quant_access::Level::Read)
		&& !state.class_denied(&record.stream, &caller.grant.deny_classes)
}

/// `POST /v1/questions/{id}/answer`: record the answer and its citations.
/// Only the client that opened the question may answer it. The id may be
/// written as it is shown (`ques_…`) or as a bare UUID.
pub async fn answer(
	AxumState(state): Shared,
	Path(id): Path<String>,
	request: Signed<quant_api::QuestionAnswer>,
) -> ApiResult<Json<quant_api::QuestionView>> {
	let id: QuestionId = id
		.parse()
		.map_err(|_| Error::Invalid("bad question id".into()))?;
	let caller = &request.caller;
	let body = request.body;
	let answer_text = clean(&body.answer, MAX_ANSWER);
	if answer_text.is_empty() {
		return Err(Error::Invalid("an answer needs text".into()));
	}
	if body.citations.len() > MAX_CITATIONS {
		return Err(Error::Invalid(format!(
			"an answer cites at most {MAX_CITATIONS} records"
		)));
	}
	state
		.vault
		.store
		.abandon_stale_questions(Timestamp::now())?;
	// The question must be the caller's own and still open; a question
	// that does not exist, belongs to someone else, or is closed all
	// answer the same way.
	let question = state.vault.store.question(id)?;
	let ok =
		question.is_some_and(|q| q.client == caller.client.id && q.status == QuestionStatus::Open);
	if !ok {
		let _ = state.vault.store.audit(
			AuditEvent::new(
				caller.actor(),
				"question.answer",
				id.to_string(),
				Outcome::Failed,
			)
			.meta(audit_meta(&caller.meta).0.as_deref(), Some(id), None),
		);
		return Err(Error::NotFound);
	}
	// Every citation must name a record this grant could read, or the
	// whole request fails as not found.
	if !body
		.citations
		.iter()
		.all(|record| citation_readable(&state, caller, *record))
	{
		let _ = state.vault.store.audit(
			AuditEvent::new(
				caller.actor(),
				"question.answer",
				id.to_string(),
				Outcome::Failed,
			)
			.meta(audit_meta(&caller.meta).0.as_deref(), Some(id), None),
		);
		return Err(Error::NotFound);
	}
	let now = Timestamp::now();
	state
		.vault
		.store
		.answer_question(id, caller.client.id, &answer_text, &body.citations, now)?;
	let (purpose, _, model) = audit_meta(&caller.meta);
	state.vault.store.audit(
		AuditEvent::new(
			caller.actor(),
			"question.answer",
			id.to_string(),
			Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.meta(purpose.as_deref(), Some(id), model.as_deref())
		.detail(serde_json::json!({
			"citations": body.citations.len(),
			"answer_chars": answer_text.chars().count(),
		})),
	)?;
	let answered = state.vault.store.question(id)?.ok_or(Error::NotFound)?;
	Ok(Json(shape(&answered, !caller.grant.unredacted)))
}

/// `GET /v1/questions/mine`: the caller's recent questions, answers and
/// all, sanitised and redacted like every other served text.
pub async fn mine(
	AxumState(state): Shared,
	request: Signed<()>,
) -> ApiResult<Json<Vec<quant_api::QuestionView>>> {
	let caller = &request.caller;
	state
		.vault
		.store
		.abandon_stale_questions(Timestamp::now())?;
	let mine = state.vault.store.questions(Some(caller.client.id), 50)?;
	Ok(Json(
		mine.iter()
			.map(|question| shape(question, !caller.grant.unredacted))
			.collect(),
	))
}

fn owner_audit(
	state: &State,
	action: &str,
	target: impl Into<String>,
	detail: serde_json::Value,
) -> ApiResult<()> {
	state.vault.store.audit(
		AuditEvent::new(
			Actor::Owner,
			action,
			target,
			Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.detail(detail),
	)?;
	Ok(())
}

/// `POST /v1/owner/questions`: every question, recent first.
pub async fn owner_list(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<Option<quant_api::QuestionListRequest>>,
) -> ApiResult<Json<Vec<quant_api::QuestionView>>> {
	let body = body.unwrap_or_default();
	let limit = body.limit.unwrap_or(50).clamp(1, 500);
	state
		.vault
		.store
		.abandon_stale_questions(Timestamp::now())?;
	let questions = state.vault.store.questions(body.client, limit)?;
	owner_audit(
		&state,
		"questions.list",
		"",
		serde_json::json!({ "count": questions.len(), "client": body.client }),
	)?;
	// The owner reads their own vault: sanitised, never redacted.
	Ok(Json(questions.iter().map(|q| shape(q, false)).collect()))
}

/// `POST /v1/owner/questions/trail`: one question, its answer, and every
/// audit entry that carried its id — what was asked for, what was served,
/// and what was withheld.
pub async fn owner_trail(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<quant_api::QuestionTrailRequest>,
) -> ApiResult<Json<quant_api::QuestionTrail>> {
	state
		.vault
		.store
		.abandon_stale_questions(Timestamp::now())?;
	let question = state
		.vault
		.store
		.question(body.id)?
		.ok_or(Error::NotFound)?;
	let entries = state.vault.store.audit_for_question(body.id, 2_000)?;
	owner_audit(
		&state,
		"questions.trail",
		body.id.to_string(),
		serde_json::json!({ "entries": entries.len() }),
	)?;
	Ok(Json(quant_api::QuestionTrail {
		question: shape(&question, false),
		entries,
	}))
}
