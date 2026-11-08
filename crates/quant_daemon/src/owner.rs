//! Owner routes: pairing, grants, purge, audit. Each needs a session opened
//! with the owner's passphrase, so a process that can reach the socket but
//! does not know the passphrase (an agent with a shell, say) cannot grant
//! itself access.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Grant;
use quant_access::Scopes;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_api::ApproveRequest;
use quant_api::Audit;
use quant_api::AuditRequest;
use quant_api::ClientView;
use quant_api::DenyRequest;
use quant_api::Hello;
use quant_api::OwnerSession;
use quant_api::PairRequest;
use quant_api::PairResponse;
use quant_api::PauseRequest;
use quant_api::PurgeRequest;
use quant_api::Purged;
use quant_api::RevokeRequest;
use quant_api::SessionSource;
use quant_api::Status;
use quant_api::UnlockRequest;
use quant_core::StreamSpec;
use quant_core::TimeRange;
use quant_store::Client;
use quant_store::ClientKind;
use quant_store::ClientStatus;

use crate::authn::Owner;
use crate::error::ApiResult;
use crate::error::Error;
use crate::query::resolve;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

fn kind_in(kind: quant_api::ClientKind) -> ClientKind {
	match kind {
		quant_api::ClientKind::Agent => ClientKind::Agent,
		quant_api::ClientKind::Cli => ClientKind::Cli,
		quant_api::ClientKind::Device => ClientKind::Device,
		quant_api::ClientKind::App => ClientKind::App,
	}
}

fn kind_out(kind: ClientKind) -> quant_api::ClientKind {
	match kind {
		ClientKind::Agent => quant_api::ClientKind::Agent,
		ClientKind::Cli => quant_api::ClientKind::Cli,
		ClientKind::Device => quant_api::ClientKind::Device,
		ClientKind::App => quant_api::ClientKind::App,
	}
}

fn status_out(status: ClientStatus) -> quant_api::ClientStatus {
	match status {
		ClientStatus::Pending => quant_api::ClientStatus::Pending,
		ClientStatus::Active => quant_api::ClientStatus::Active,
		ClientStatus::Revoked => quant_api::ClientStatus::Revoked,
	}
}

pub(crate) fn owner_audit(
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

/// `GET /v1/hello`
pub async fn hello(AxumState(state): Shared) -> Json<Hello> {
	let relay = crate::relay::config(&state.vault.store).ok().flatten();
	Json(Hello {
		service: "quant".into(),
		version: env!("CARGO_PKG_VERSION").into(),
		vault_id: state.vault.manifest.vault_id.clone(),
		tls_fingerprint: state
			.tls_fingerprint
			.read()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.clone(),
		relay: relay.as_ref().map(|config| config.url.to_string()),
		relay_rid: relay.is_some().then(|| state.relay_key.rid()),
	})
}

/// `POST /v1/pair`. Unauthenticated: it only ever creates a pending client
/// with no access, and the number waiting is capped.
pub async fn pair(
	AxumState(state): Shared,
	Json(body): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
	let scopes = Scopes::parse(&body.scopes)?;
	let client = state.vault.store.register_client(
		&body.name,
		kind_in(body.kind),
		body.public_key,
		&scopes,
	)?;
	if client.status == ClientStatus::Pending
		&& client.created_at > Timestamp::now() - SignedDuration::from_secs(5)
	{
		state.vault.store.audit(
			AuditEvent::new(
				Actor::Unknown {
					claimed: Some(client.name.clone()),
				},
				"pair.request",
				client.id.to_string(),
				Outcome::Allowed {
					records: 0,
					bytes: 0,
				},
			)
			.detail(
				serde_json::json!({ "scopes": scopes.to_strings(), "code": client.pairing_code() }),
			),
		)?;
	}
	Ok(Json(PairResponse {
		client: client.id,
		pairing_code: client.pairing_code(),
		status: status_out(client.status),
	}))
}

/// `POST /v1/owner/unlock`
pub async fn unlock(
	AxumState(state): Shared,
	Json(body): Json<UnlockRequest>,
) -> ApiResult<Json<OwnerSession>> {
	let now = Timestamp::now();
	if let Some(wait) = state.owner().wait(now) {
		return Err(Error::Locked(wait));
	}
	let check = Arc::clone(&state);
	let ok = tokio::task::spawn_blocking(move || check.vault.check_passphrase(&body.passphrase))
		.await
		.map_err(|e| Error::Internal(e.to_string()))?;
	if !ok {
		state.owner().fail(Timestamp::now());
		state.vault.store.audit(AuditEvent::new(
			Actor::Unknown {
				claimed: Some("owner".into()),
			},
			"owner.unlock",
			"",
			Outcome::Denied {
				reason: quant_access::Denied::OutOfScope,
			},
		))?;
		return Err(Error::Unauthenticated("wrong passphrase".into()));
	}
	let (token, expires_at) = state
		.owner()
		.open(SessionSource::Passphrase, Timestamp::now());
	owner_audit(&state, "owner.unlock", "", serde_json::Value::Null)?;
	Ok(Json(OwnerSession { token, expires_at }))
}

/// `POST /v1/owner/lock`
pub async fn lock(AxumState(state): Shared, owner: Owner) -> ApiResult<Json<()>> {
	state.owner().close(&owner.token);
	Ok(Json(()))
}

fn view(state: &State, client: Client) -> ApiResult<ClientView> {
	let grant = state.vault.store.active_grant(client.id)?;
	Ok(ClientView {
		pairing_code: client.pairing_code(),
		id: client.id,
		name: client.name,
		kind: kind_out(client.kind),
		status: status_out(client.status),
		requested: client.requested.to_strings(),
		created_at: client.created_at,
		last_seen: client.last_seen,
		grant,
	})
}

/// `GET /v1/owner/clients`
pub async fn clients(AxumState(state): Shared, _owner: Owner) -> ApiResult<Json<Vec<ClientView>>> {
	let clients = state.vault.store.clients()?;
	Ok(Json(
		clients
			.into_iter()
			.map(|c| view(&state, c))
			.collect::<ApiResult<_>>()?,
	))
}

/// `GET /v1/owner/grants`
pub async fn grants(AxumState(state): Shared, _owner: Owner) -> ApiResult<Json<Vec<Grant>>> {
	Ok(Json(state.vault.store.grants()?))
}

fn find_client(state: &State, needle: &str) -> ApiResult<Client> {
	if let Some(client) = state.vault.store.find_pending(needle)? {
		return Ok(client);
	}
	let code = needle.trim().to_ascii_uppercase().replace('-', "");
	if let Some(client) = state
		.vault
		.store
		.clients()?
		.into_iter()
		.find(|c| c.pairing_code().replace('-', "") == code)
	{
		return Ok(client);
	}
	if let Ok(id) = needle.parse() {
		return state.vault.store.client(id)?.ok_or(Error::NotFound);
	}
	Err(Error::NotFound)
}

/// `POST /v1/owner/approve`
pub async fn approve(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<ApproveRequest>,
) -> ApiResult<Json<ClientView>> {
	let client = find_client(&state, &body.client)?;
	if client.status == ClientStatus::Revoked {
		return Err(Error::Invalid(
			"this client was revoked; it must pair again with a new key".into(),
		));
	}
	let scopes = match &body.scopes {
		Some(scopes) => Scopes::parse(scopes)?,
		None => client.requested.clone(),
	};
	if scopes.0.is_empty() {
		return Err(Error::Invalid("a grant needs at least one scope".into()));
	}
	let mut grant = Grant::new(
		client.id,
		body.label.clone().unwrap_or_else(|| client.name.clone()),
		scopes,
	);
	grant.lookback = body
		.lookback_hours
		.map(|h| SignedDuration::from_hours(i64::from(h)));
	grant.expires_at = body
		.expires_in_hours
		.map(|h| grant.created_at + SignedDuration::from_hours(i64::from(h)));
	if let Some(limits) = body.limits {
		grant.limits = limits;
	}
	grant.unredacted = body.unredacted;
	if let Some(deny_classes) = body.deny_classes {
		grant.deny_classes = deny_classes;
	}
	state.vault.store.approve(&grant)?;
	state.limiter().reset(grant.id);
	owner_audit(
		&state,
		"grant.create",
		grant.id.to_string(),
		serde_json::json!({ "client": client.id, "scopes": grant.scopes.to_strings(), "lookback_hours": body.lookback_hours, "expires_in_hours": body.expires_in_hours, "limits": grant.limits, "unredacted": grant.unredacted, "deny_classes": grant.deny_classes }),
	)?;
	let client = state
		.vault
		.store
		.client(client.id)?
		.ok_or(Error::NotFound)?;
	Ok(Json(view(&state, client)?))
}

/// `POST /v1/owner/deny`
pub async fn deny(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<DenyRequest>,
) -> ApiResult<Json<()>> {
	let client = find_client(&state, &body.client)?;
	state
		.vault
		.store
		.set_client_status(client.id, ClientStatus::Revoked)?;
	owner_audit(
		&state,
		"pair.deny",
		client.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(()))
}

/// `POST /v1/owner/revoke`
pub async fn revoke(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<RevokeRequest>,
) -> ApiResult<Json<Grant>> {
	let grant_id = if let Ok(id) = body.id.parse() {
		if state.vault.store.grants()?.iter().any(|g| g.id == id) {
			Some(id)
		} else {
			None
		}
	} else {
		None
	};
	let grant_id = if let Some(id) = grant_id {
		id
	} else {
		let client = find_client(&state, &body.id)?;
		state
			.vault
			.store
			.active_grant(client.id)?
			.ok_or(Error::NotFound)?
			.id
	};
	let grant = state.vault.store.revoke_grant(grant_id)?;
	state.limiter().reset(grant.id);
	owner_audit(
		&state,
		"grant.revoke",
		grant.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(grant))
}

/// `POST /v1/owner/purge`
pub async fn purge(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<PurgeRequest>,
) -> ApiResult<Json<Purged>> {
	let range = TimeRange::new(body.from, body.to).map_err(|e| Error::Invalid(e.to_string()))?;
	let everything = Grant::new(
		quant_core::ClientId::new(),
		"owner",
		Scopes::parse(["*:raw"])?,
	);
	let specs = state.specs_lock();
	let (selector, _) = resolve(
		&everything,
		&specs,
		&body.streams,
		quant_access::Level::Summary,
	)?;
	drop(specs);
	let (records, series, derived) = state.vault.store.purge(&selector, range)?;
	let media = state.vault.sweep_media(Timestamp::now())?;
	owner_audit(
		&state,
		"purge",
		body.streams.join(","),
		serde_json::json!({ "from": range.start, "to": range.end, "records": records, "series": series, "derived": derived, "media": media }),
	)?;
	Ok(Json(Purged {
		records,
		series,
		derived,
		media,
	}))
}

/// `POST /v1/owner/audit`
pub async fn audit(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<Option<AuditRequest>>,
) -> ApiResult<Json<Audit>> {
	let body = body.unwrap_or_default();
	let since = body
		.since
		.unwrap_or_else(|| Timestamp::now() - SignedDuration::from_hours(24));
	let limit = body.limit.unwrap_or(200).clamp(1, 2000);
	let entries = if body.newest {
		let mut tail = state.vault.store.audit_tail(limit)?;
		tail.reverse();
		tail
	} else {
		state.vault.store.audit_log(since, limit)?
	};
	let chain = state.vault.store.verify_audit()?;
	Ok(Json(Audit {
		entries,
		chain_ok: Some(chain.is_ok()),
		chain_len: chain.ok(),
	}))
}

/// `POST /v1/owner/streams`
pub async fn register_stream(
	AxumState(state): Shared,
	_owner: Owner,
	Json(spec): Json<StreamSpec>,
) -> ApiResult<Json<StreamSpec>> {
	if spec.kind == quant_core::StreamKind::Series && spec.channels.is_empty() {
		return Err(Error::Invalid("a series stream needs channels".into()));
	}
	state.register_stream(spec.clone())?;
	owner_audit(
		&state,
		"stream.register",
		spec.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(spec))
}

/// `GET /v1/owner/derive`
pub async fn derive_config(
	AxumState(state): Shared,
	_owner: Owner,
) -> ApiResult<Json<quant_derive::Config>> {
	let config = state
		.vault
		.store
		.meta("derive.config")?
		.and_then(|json| serde_json::from_str(&json).ok())
		.unwrap_or_default();
	Ok(Json(config))
}

/// `POST /v1/owner/derive`
pub async fn set_derive_config(
	AxumState(state): Shared,
	_owner: Owner,
	Json(config): Json<quant_derive::Config>,
) -> ApiResult<Json<quant_derive::Config>> {
	let json = serde_json::to_string(&config).map_err(|e| Error::Invalid(e.to_string()))?;
	state.vault.store.set_meta("derive.config", &json)?;
	owner_audit(
		&state,
		"derive.configure",
		"",
		serde_json::to_value(&config).unwrap_or_default(),
	)?;
	Ok(Json(config))
}

/// `GET /v1/owner/status`
pub async fn status(AxumState(state): Shared, owner: Owner) -> ApiResult<Json<Status>> {
	let clients = state.vault.store.clients()?;
	Ok(Json(Status {
		version: env!("CARGO_PKG_VERSION").into(),
		vault_id: state.vault.manifest.vault_id.clone(),
		session: owner.via,
		started_at: state.started_at,
		capture: state.capture_status(),
		pending_clients: clients
			.iter()
			.filter(|c| c.status == ClientStatus::Pending)
			.count(),
		active_grants: clients
			.iter()
			.filter(|c| c.status == ClientStatus::Active)
			.count(),
		paused: state.capture_control.paused(),
	}))
}

/// `GET /v1/owner/capture`
pub async fn capture_config(
	AxumState(state): Shared,
	_owner: Owner,
) -> ApiResult<Json<quant_capture_macos::CaptureConfig>> {
	Ok(Json(crate::capture::config(&state)?))
}

/// `POST /v1/owner/capture`
pub async fn set_capture_config(
	AxumState(state): Shared,
	_owner: Owner,
	Json(config): Json<quant_capture_macos::CaptureConfig>,
) -> ApiResult<Json<quant_capture_macos::CaptureConfig>> {
	let setting = Arc::clone(&state);
	let stored = config.clone();
	// Restarting waits for the sources to flush, which can take a moment.
	tokio::task::spawn_blocking(move || crate::capture::set_config(&setting, &stored))
		.await
		.map_err(|e| Error::Internal(e.to_string()))??;
	owner_audit(
		&state,
		"capture.configure",
		"",
		serde_json::to_value(&config).unwrap_or_default(),
	)?;
	Ok(Json(config))
}

/// `POST /v1/owner/capture/pause`
pub async fn pause_capture(
	AxumState(state): Shared,
	owner: Owner,
	Json(request): Json<PauseRequest>,
) -> ApiResult<Json<Status>> {
	let changing = Arc::clone(&state);
	let until = request
		.minutes
		.map(|m| Timestamp::now() + SignedDuration::from_mins(i64::from(m)));
	tokio::task::spawn_blocking(move || {
		if request.paused {
			crate::capture::pause(&changing, until);
			Ok(())
		} else {
			crate::capture::resume(&changing)
		}
	})
	.await
	.map_err(|e| Error::Internal(e.to_string()))??;
	if let (true, Some(minutes)) = (request.paused, request.minutes) {
		let waking = Arc::clone(&state);
		tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_mins(u64::from(minutes))).await;
			let _ = tokio::task::spawn_blocking(move || {
				crate::capture::tick(&waking, Timestamp::now())
			})
			.await;
		});
	}
	owner_audit(
		&state,
		if request.paused {
			"capture.pause"
		} else {
			"capture.resume"
		},
		"",
		serde_json::to_value(request).unwrap_or_default(),
	)?;
	status(AxumState(state), owner).await
}

/// `POST /v1/audit`: a client with `audit:read` sees its own entries.
pub async fn client_audit(
	AxumState(state): Shared,
	request: crate::authn::Signed<Option<AuditRequest>>,
) -> ApiResult<Json<Audit>> {
	let caller = &request.caller;
	caller.grant.authorize_audit(Timestamp::now())?;
	let body = request.body.unwrap_or_default();
	let since = body
		.since
		.unwrap_or_else(|| Timestamp::now() - SignedDuration::from_hours(24));
	let mine = caller.actor();
	let entries = state
		.vault
		.store
		.audit_log(since, 20_000)?
		.into_iter()
		.filter(|e| e.actor == mine)
		.take(body.limit.unwrap_or(200).clamp(1, 2000))
		.collect();
	Ok(Json(Audit {
		entries,
		chain_ok: None,
		chain_len: None,
	}))
}
