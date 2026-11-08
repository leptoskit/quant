//! The memory-about-the-owner routes: what an agent may learn about who the
//! owner is, shaped by its grant.
//!
//! Claims live behind the built-in stream `memory.profile`, so existing
//! scopes apply: `memory.profile:summary` serves only what the owner stated
//! themselves, `memory.profile:read` everything active with evidence, and
//! `memory.profile:write` lets an agent propose claims for the owner to
//! approve.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Denied;
use quant_access::Level;
use quant_access::redact;
use quant_api::ClaimView;
use quant_api::Context;
use quant_api::ContextRequest;
use quant_api::ContextSection;
use quant_api::ProfileClaims;
use quant_api::ProposeRequest;
use quant_api::Proposed;
use quant_api::ServedRecord;
use quant_core::Authority;
use quant_core::Claim;
use quant_core::ClaimId;
use quant_core::ClaimKind;
use quant_core::ClaimStatus;
use quant_core::RecordId;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::ClaimFilter;
use quant_store::RecordQuery;
use quant_store::SearchHit;
use quant_store::Selector;

use crate::authn::Caller;
use crate::authn::Signed;
use crate::error::ApiResult;
use crate::error::Error;
use crate::query::deny;
use crate::query::resolve;
use crate::query::respond;
use crate::query::serve;
use crate::query::window;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// The built-in stream claims are served as.
pub const PROFILE: &str = "memory.profile";

/// The most proposals one grant may make per hour.
pub const MAX_PROPOSALS_PER_HOUR: u64 = 20;

/// The budget a context request defaults to, in rough tokens.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 1_500;

fn profile_stream() -> StreamId {
	PROFILE
		.parse()
		.expect("the built-in profile stream id is valid")
}

/// Serve the active claims `grant` may see at its `memory.profile` level.
/// Text is sanitised and redacted like every other served text; claims
/// carrying a denied class are never included.
fn shape_claims(state: &State, caller: &Caller, level: Level, claims: &[Claim]) -> Vec<ClaimView> {
	let grant = &caller.grant;
	let redact = !grant.unredacted;
	claims
		.iter()
		.filter(|claim| claim.status == ClaimStatus::Active)
		.filter(|claim| !grant.denies(&claim.classes))
		.filter(|claim| {
			match level {
				Level::Summary => {
					matches!(claim.authority, Authority::Owner | Authority::OwnerAuthored)
				}
				Level::Read | Level::Raw => true,
			}
		})
		.map(|claim| {
			ClaimView {
				id: claim.id,
				subject: redact::prepare(&claim.subject, redact),
				kind: claim.kind,
				statement: redact::prepare(&claim.statement, redact),
				topic: claim
					.topic
					.as_deref()
					.map(|topic| redact::prepare(topic, redact)),
				authority: claim.authority,
				confidence: claim.confidence,
				classes: claim.classes.clone(),
				status: claim.status,
				valid_from: claim.valid_from,
				valid_to: claim.valid_to,
				evidence: match level {
					Level::Summary => Vec::new(),
					Level::Read | Level::Raw => {
						claim
							.evidence
							.iter()
							.filter(|id| evidence_readable(state, caller, **id))
							.copied()
							.collect()
					}
				},
				created_at: claim.created_at,
				updated_at: claim.updated_at,
			}
		})
		.collect()
}

/// Whether the grant could read the record `id` at read level: the record
/// exists, its stream is in scope at read level or above, and its classes
/// are not denied. Unreadable ids are dropped, never named.
fn evidence_readable(state: &State, caller: &Caller, id: RecordId) -> bool {
	let Ok(Some(record)) = state.vault.store.record(id) else {
		return false;
	};
	caller.grant.scopes.read_level(&record.stream) >= Some(Level::Read)
		&& !state.class_denied(&record.stream, &caller.grant.deny_classes)
}

/// The claims a grant may see, at the level `memory.profile` allows.
pub(crate) fn profile_for(state: &State, caller: &Caller) -> ApiResult<(Level, Vec<ClaimView>)> {
	let level = caller
		.grant
		.scopes
		.read_level(&profile_stream())
		.ok_or(Error::Forbidden(Denied::OutOfScope))?;
	if state.class_denied(&profile_stream(), &caller.grant.deny_classes) {
		return Err(Error::Forbidden(Denied::OutOfScope));
	}
	let claims = state
		.vault
		.store
		.claims(&ClaimFilter::new().status(ClaimStatus::Active))?;
	Ok((level, shape_claims(state, caller, level, &claims)))
}

/// `GET /v1/memory/profile`
pub async fn profile(
	AxumState(state): Shared,
	request: Signed<()>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let (level, claims) = profile_for(&state, caller)
		.map_err(|error| deny(&state, caller, "claims.read", PROFILE, error))?;
	let answer = ProfileClaims { level, claims };
	respond(
		&state,
		caller,
		"claims.read",
		PROFILE.into(),
		serde_json::json!({ "level": answer.level }),
		answer.claims.len(),
		&answer,
	)
}

/// `POST /v1/memory/proposals`
pub async fn propose(
	AxumState(state): Shared,
	request: Signed<ProposeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body;
	let now = Timestamp::now();
	if body.claims.is_empty() {
		return Err(Error::Invalid("a proposal needs at least one claim".into()));
	}
	if body.claims.len() > 20 {
		return Err(Error::Invalid(
			"at most 20 claims per proposal request".into(),
		));
	}
	if let Err(denied) = caller.grant.authorize_write(&profile_stream(), now) {
		return Err(deny(
			&state,
			caller,
			"claims.propose",
			PROFILE,
			denied.into(),
		));
	}
	// At most 20 proposals per grant per hour.
	let hour_ago = now - SignedDuration::from_hours(1);
	let recent = state
		.vault
		.store
		.proposal_count(caller.client.id, hour_ago)?;
	let room = MAX_PROPOSALS_PER_HOUR.saturating_sub(recent);
	if u64::try_from(body.claims.len()).unwrap_or(0) > room {
		return Err(deny(
			&state,
			caller,
			"claims.propose",
			PROFILE,
			Error::Forbidden(Denied::RateLimited),
		));
	}
	// Check every proposal before storing any, so a request either lands
	// whole or not at all.
	let failed = || {
		state
			.vault
			.store
			.audit(quant_access::audit::AuditEvent::new(
				caller.actor(),
				"claims.propose",
				PROFILE,
				quant_access::audit::Outcome::Failed,
			))
	};
	let mut claims = Vec::with_capacity(body.claims.len());
	for proposal in &body.claims {
		// Every evidence id must exist and be readable by this grant at
		// read level; otherwise the request fails as not found, never
		// saying which id or why.
		if proposal.evidence.is_empty()
			|| !proposal
				.evidence
				.iter()
				.all(|evidence| evidence_readable(&state, caller, *evidence))
		{
			failed()?;
			return Err(Error::NotFound);
		}
		let claim = Claim {
			id: ClaimId::new(),
			subject: proposal.subject.clone().unwrap_or_else(|| "me".to_owned()),
			kind: proposal.kind.unwrap_or(ClaimKind::Fact),
			statement: proposal.statement.clone(),
			topic: proposal.topic.clone(),
			authority: Authority::Inferred,
			confidence: proposal.confidence.unwrap_or(0.5),
			classes: proposal.classes.clone(),
			status: ClaimStatus::Held,
			valid_from: None,
			valid_to: None,
			evidence: proposal.evidence.clone(),
			proposed_by: Some(caller.client.id),
			created_at: now,
			updated_at: now,
			superseded_by: None,
		};
		if let Err(error) = claim.validate() {
			failed()?;
			return Err(Error::Invalid(error.to_string()));
		}
		claims.push(claim);
	}
	let mut ids = Vec::with_capacity(claims.len());
	for claim in &claims {
		state.vault.store.insert_claim(claim)?;
		ids.push(claim.id);
	}
	let answer = Proposed { claims: ids };
	respond(
		&state,
		caller,
		"claims.propose",
		PROFILE.into(),
		serde_json::json!({ "count": answer.claims.len() }),
		answer.claims.len(),
		&answer,
	)
}

/// Rough token count of a text: characters / 4.
fn tokens(text: &str) -> u32 {
	#[allow(clippy::cast_possible_truncation)]
	let count = text.chars().count().max(1).div_ceil(4) as u32;
	count
}

fn claim_tokens(claim: &ClaimView) -> u32 {
	let mut cost = tokens(&claim.statement);
	if let Some(topic) = &claim.topic {
		cost += tokens(topic);
	}
	cost
}

/// Take items while `budget` and the record `allowance` last, reporting
/// whether the run was cut short.
fn fill<T, F: Fn(&T) -> u32>(
	items: impl IntoIterator<Item = T>,
	cost: F,
	budget: &mut u32,
	allowance: &mut usize,
) -> (Vec<T>, bool) {
	let mut out = Vec::new();
	let mut cut = false;
	for item in items {
		let item_tokens = cost(&item);
		if *allowance == 0 || item_tokens > *budget {
			cut = true;
			break;
		}
		*budget -= item_tokens;
		*allowance -= 1;
		out.push(item);
	}
	(out, cut)
}

fn section<T>(items: Vec<T>, withheld: bool, cut: bool) -> ContextSection<T> {
	ContextSection {
		included: !items.is_empty(),
		withheld,
		cut,
		count: items.len(),
		items,
	}
}

fn searchable(state: &State, caller: &Caller) -> ApiResult<Selector> {
	let specs = state.specs_lock();
	let (selector, _) = resolve(&caller.grant, &specs, &[], Level::Read)?;
	Ok(selector)
}

/// `POST /v1/context`
pub async fn context(
	AxumState(state): Shared,
	request: Signed<ContextRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body;
	let task = body.task.trim();
	if task.is_empty() {
		return Err(Error::Invalid("context needs a task".into()));
	}
	if task.chars().count() > 2_000 {
		return Err(Error::Invalid(
			"the task is at most 2,000 characters".into(),
		));
	}
	let budget = body
		.budget_tokens
		.unwrap_or(DEFAULT_CONTEXT_TOKENS)
		.clamp(200, 8_000);
	let mut left = budget;
	let allowance =
		state
			.limiter()
			.record_allowance(caller.grant.id, &caller.grant.limits, Timestamp::now());
	let mut item_allowance = allowance;

	// The task's embedding, when an embedding space exists and holds
	// vectors. One call serves both the claims and the records sections;
	// without it each falls back to keyword search.
	let space = crate::learning::space(&state)?;
	let task_vec = match &space {
		Some(space)
			if state.vault.store.has_embeddings(space)?
				|| state.vault.store.has_claim_embeddings(space)? =>
		{
			crate::learning::embed_query(&state, task).await.ok()
		}
		_ => None,
	};

	// (a) Profile claims: those matching the task's terms first, then the
	// rest by authority and confidence. With embeddings, the keyword and
	// vector rankings are fused.
	let profile_section = match profile_for(&state, caller) {
		Ok((level, _)) => {
			let filter = ClaimFilter::new()
				.status(ClaimStatus::Active)
				.exclude_classes(&caller.grant.deny_classes);
			let all = state.vault.store.claims(&filter)?;
			let ordered: Vec<Claim> = match &task_vec {
				Some(vector)
					if space.as_deref().is_some_and(|space| {
						state
							.vault
							.store
							.has_claim_embeddings(space)
							.unwrap_or(false)
					}) =>
				{
					let space = space.clone().expect("the space was just checked");
					let keyword = match quant_store::fts_any(task) {
						Some(query) => state.vault.store.claims_matching(&filter, &query)?,
						None => Vec::new(),
					};
					let similar = state
						.vault
						.store
						.similar_claims(&space, &filter, vector, 200)?;
					let keyword_ids: Vec<ClaimId> = keyword.iter().map(|claim| claim.id).collect();
					let similar_ids: Vec<ClaimId> = similar.iter().map(|claim| claim.id).collect();
					let mut by_id: HashMap<ClaimId, Claim> = keyword
						.into_iter()
						.chain(similar)
						.map(|claim| (claim.id, claim))
						.collect();
					let mut fused: Vec<Claim> =
						quant_store::rrf_fuse(&keyword_ids, &similar_ids, 60)
							.into_iter()
							.filter_map(|(id, _)| by_id.remove(&id))
							.collect();
					let fused_ids: HashSet<ClaimId> = fused.iter().map(|claim| claim.id).collect();
					let mut rest: Vec<Claim> = all
						.into_iter()
						.filter(|claim| !fused_ids.contains(&claim.id))
						.collect();
					rest.sort_by(|a, b| {
						b.authority
							.cmp(&a.authority)
							.then(b.confidence.total_cmp(&a.confidence))
					});
					fused.extend(rest);
					fused
				}
				_ => {
					let mut matching: Vec<Claim> = match quant_store::fts_any(task) {
						Some(query) => state.vault.store.claims_matching(&filter, &query)?,
						None => Vec::new(),
					};
					let matched: Vec<ClaimId> = matching.iter().map(|claim| claim.id).collect();
					let mut rest: Vec<Claim> = all
						.into_iter()
						.filter(|claim| !matched.contains(&claim.id))
						.collect();
					rest.sort_by(|a, b| {
						b.authority
							.cmp(&a.authority)
							.then(b.confidence.total_cmp(&a.confidence))
					});
					matching.extend(rest);
					matching
				}
			};
			let views: Vec<ClaimView> = shape_claims(&state, caller, level, &ordered);
			let (items, cut) = fill(views, claim_tokens, &mut left, &mut item_allowance);
			section(items, false, cut)
		}
		Err(_) => section(Vec::new(), true, false),
	};

	// (b) The latest memory.daily and the last 24 hours of memory.episode
	// texts, only where the grant may read them.
	let memory_section = {
		let daily: StreamId = "memory.daily".parse().expect("built-in stream");
		let episode: StreamId = "memory.episode".parse().expect("built-in stream");
		let readable = |stream: &StreamId| {
			caller.grant.scopes.read_level(stream) >= Some(Level::Read)
				&& !state.class_denied(stream, &caller.grant.deny_classes)
		};
		if !readable(&daily) || !readable(&episode) {
			section(Vec::new(), true, false)
		} else {
			let now = Timestamp::now();
			let floor = caller.grant.floor(now);
			let mut records: Vec<quant_core::Record> = state.vault.store.records(
				&RecordQuery::new(
					Selector::stream(daily),
					TimeRange::new(floor, now).map_err(|e| Error::Invalid(e.to_string()))?,
				)
				.limit(1),
			)?;
			let day = window(&caller.grant, None, None)?;
			records.extend(
				state.vault.store.records(
					&RecordQuery::new(Selector::stream(episode), day)
						.limit(100)
						.oldest_first(),
				)?,
			);
			let served: Vec<ServedRecord> = records
				.into_iter()
				.map(|record| serve(record, None, &caller.grant))
				.collect();
			let cost = |record: &ServedRecord| record.text.as_deref().map_or(0, tokens);
			let (items, cut) = fill(served, cost, &mut left, &mut item_allowance);
			section(items, false, cut)
		}
	};

	// (c) Records matching the task's terms, through the search path: hybrid
	// when the task has an embedding and the space holds vectors.
	let records_section = match searchable(&state, caller) {
		Ok(selector) if !selector.is_empty() => {
			let range = window(&caller.grant, None, None)?;
			let query = RecordQuery::new(selector, range).search(task).limit(500);
			let hits: Vec<SearchHit> = match (&space, &task_vec) {
				(Some(space), Some(vector)) => state.vault.store.hybrid(&query, space, vector)?,
				_ => state.vault.store.search(&query)?,
			};
			let cost = |hit: &SearchHit| {
				hit.record.text.as_deref().map_or(0, tokens) + tokens(&hit.snippet)
			};
			let (items, cut) = fill(hits, cost, &mut left, &mut item_allowance);
			let served: Vec<ServedRecord> = items
				.into_iter()
				.map(|hit| {
					let snippet = (!hit.snippet.is_empty()).then_some(hit.snippet);
					serve(hit.record, snippet, &caller.grant)
				})
				.collect();
			section(served, false, cut)
		}
		_ => section(Vec::new(), true, false),
	};

	let used = budget - left;
	let answer = Context {
		budget_tokens: budget,
		used_tokens: used,
		profile: profile_section,
		memory: memory_section,
		records: records_section,
	};
	let count = answer.profile.count + answer.memory.count + answer.records.count;
	respond(
		&state,
		caller,
		"context",
		task.into(),
		serde_json::json!({
			"budget_tokens": answer.budget_tokens,
			"used_tokens": answer.used_tokens,
			"profile": { "count": answer.profile.count, "withheld": answer.profile.withheld, "cut": answer.profile.cut },
			"memory": { "count": answer.memory.count, "withheld": answer.memory.withheld, "cut": answer.memory.cut },
			"records": { "count": answer.records.count, "withheld": answer.records.withheld, "cut": answer.records.cut },
		}),
		count,
		&answer,
	)
}
