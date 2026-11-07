//! Owner routes for claims: stating, listing, approving, rejecting,
//! retracting, and correcting what Quant believes about its owner. Each
//! needs a session opened with the passphrase, like every owner action, and
//! is audited.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::Timestamp;
use quant_api::ClaimActionRequest;
use quant_api::ClaimCorrectRequest;
use quant_api::ClaimListRequest;
use quant_api::OwnerClaimRequest;
use quant_core::Authority;
use quant_core::Claim;
use quant_core::ClaimKind;
use quant_core::ClaimStatus;
use quant_store::ClaimFilter;

use crate::authn::Owner;
use crate::error::ApiResult;
use crate::error::Error;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

fn owner_audit(
	state: &State,
	action: &str,
	target: impl Into<String>,
	detail: serde_json::Value,
) -> ApiResult<()> {
	state.vault.store.audit(
		quant_access::audit::AuditEvent::new(
			quant_access::audit::Actor::Owner,
			action,
			target,
			quant_access::audit::Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.detail(detail),
	)?;
	Ok(())
}

fn status_of(state: &State, id: quant_core::ClaimId, want: ClaimStatus) -> ApiResult<Claim> {
	let claim = state.vault.store.claim(id)?.ok_or(Error::NotFound)?;
	if claim.status != want {
		return Err(Error::Invalid(format!(
			"claim {id} is {}, not {}",
			claim.status.as_str(),
			want.as_str()
		)));
	}
	Ok(claim)
}

/// `POST /v1/owner/memory/claims`: the owner states a claim.
pub async fn tell(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<OwnerClaimRequest>,
) -> ApiResult<Json<Claim>> {
	let subject = body.subject.unwrap_or_else(|| "me".to_owned());
	let claim = Claim {
		id: quant_core::ClaimId::new(),
		subject: subject.clone(),
		kind: body.kind.unwrap_or(ClaimKind::Preference),
		statement: body.statement,
		topic: body.topic.clone(),
		authority: Authority::Owner,
		confidence: 1.0,
		classes: body.classes,
		status: ClaimStatus::Active,
		valid_from: None,
		valid_to: None,
		evidence: Vec::new(),
		proposed_by: None,
		created_at: Timestamp::now(),
		updated_at: Timestamp::now(),
		superseded_by: None,
	};
	claim
		.validate()
		.map_err(|e| Error::Invalid(e.to_string()))?;
	state.vault.store.insert_claim(&claim)?;
	// A claim with a topic supersedes everything active on the same subject
	// and topic that did not come from the owner directly.
	if let Some(topic) = &claim.topic {
		let older = state.vault.store.claims(
			&ClaimFilter::new()
				.status(ClaimStatus::Active)
				.subject(&subject)
				.topic(topic),
		)?;
		for old in older {
			if old.id != claim.id && old.authority != Authority::Owner {
				state.vault.store.supersede(old.id, claim.id)?;
			}
		}
	}
	owner_audit(
		&state,
		"claims.tell",
		claim.id.to_string(),
		serde_json::json!({ "kind": claim.kind, "topic": claim.topic, "subject": claim.subject, "classes": claim.classes }),
	)?;
	Ok(Json(claim))
}

/// `POST /v1/owner/memory/claims/list`
pub async fn list(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<Option<ClaimListRequest>>,
) -> ApiResult<Json<Vec<Claim>>> {
	let body = body.unwrap_or_default();
	let filter = ClaimFilter {
		status: body.status,
		authority: body.authority,
		kind: body.kind,
		subject: body.subject.clone(),
		topic: body.topic.clone(),
		exclude_classes: Vec::new(),
		limit: body.limit.unwrap_or(0),
	};
	let claims = state.vault.store.claims(&filter)?;
	owner_audit(
		&state,
		"claims.list",
		"",
		serde_json::json!({ "count": claims.len(), "status": filter.status, "authority": filter.authority, "kind": filter.kind, "subject": filter.subject, "topic": filter.topic }),
	)?;
	Ok(Json(claims))
}

/// `POST /v1/owner/memory/claims/approve`: a held proposal becomes active,
/// keeping its inferred authority.
pub async fn approve(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<ClaimActionRequest>,
) -> ApiResult<Json<Claim>> {
	let claim = status_of(&state, body.id, ClaimStatus::Held)?;
	state
		.vault
		.store
		.set_claim_status(body.id, ClaimStatus::Active)?;
	owner_audit(
		&state,
		"claims.approve",
		claim.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(
		state.vault.store.claim(body.id)?.ok_or(Error::NotFound)?,
	))
}

/// `POST /v1/owner/memory/claims/reject`
pub async fn reject(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<ClaimActionRequest>,
) -> ApiResult<Json<Claim>> {
	let claim = status_of(&state, body.id, ClaimStatus::Held)?;
	state
		.vault
		.store
		.set_claim_status(body.id, ClaimStatus::Rejected)?;
	owner_audit(
		&state,
		"claims.reject",
		claim.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(
		state.vault.store.claim(body.id)?.ok_or(Error::NotFound)?,
	))
}

/// `POST /v1/owner/memory/claims/retract`: the owner withdraws an active or
/// held claim.
pub async fn retract(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<ClaimActionRequest>,
) -> ApiResult<Json<Claim>> {
	let claim = state.vault.store.claim(body.id)?.ok_or(Error::NotFound)?;
	if !matches!(claim.status, ClaimStatus::Active | ClaimStatus::Held) {
		return Err(Error::Invalid(format!(
			"claim {} is {} and cannot be retracted",
			claim.id,
			claim.status.as_str()
		)));
	}
	state
		.vault
		.store
		.set_claim_status(body.id, ClaimStatus::Retracted)?;
	owner_audit(
		&state,
		"claims.retract",
		claim.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(
		state.vault.store.claim(body.id)?.ok_or(Error::NotFound)?,
	))
}

/// `POST /v1/owner/memory/claims/correct`: a new owner claim, keeping the
/// old one's subject, kind, and topic, supersedes the old one.
pub async fn correct(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<ClaimCorrectRequest>,
) -> ApiResult<Json<Claim>> {
	let old = state.vault.store.claim(body.id)?.ok_or(Error::NotFound)?;
	if !matches!(old.status, ClaimStatus::Active | ClaimStatus::Held) {
		return Err(Error::Invalid(format!(
			"claim {} is {} and cannot be corrected",
			old.id,
			old.status.as_str()
		)));
	}
	let now = Timestamp::now();
	let claim = Claim {
		id: quant_core::ClaimId::new(),
		subject: old.subject.clone(),
		kind: old.kind,
		statement: body.statement,
		topic: old.topic.clone(),
		authority: Authority::Owner,
		confidence: 1.0,
		classes: old.classes.clone(),
		status: ClaimStatus::Active,
		valid_from: old.valid_from,
		valid_to: old.valid_to,
		evidence: Vec::new(),
		proposed_by: None,
		created_at: now,
		updated_at: now,
		superseded_by: None,
	};
	claim
		.validate()
		.map_err(|e| Error::Invalid(e.to_string()))?;
	state.vault.store.insert_claim(&claim)?;
	state.vault.store.supersede(old.id, claim.id)?;
	owner_audit(
		&state,
		"claims.correct",
		claim.id.to_string(),
		serde_json::json!({ "supersedes": old.id, "subject": claim.subject, "kind": claim.kind, "topic": claim.topic }),
	)?;
	Ok(Json(claim))
}
