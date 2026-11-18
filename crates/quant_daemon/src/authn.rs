use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::http::request::Parts;
use jiff::Timestamp;
use quant_access::Grant;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_access::auth::RequestMeta;
use quant_access::auth::SignedHeaders;
use quant_access::auth::Verify;
use quant_api::SessionSource;
use quant_api::routes::OWNER_HEADER;
use quant_core::QuestionId;
use quant_store::Client;
use quant_store::ClientKind;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::query::audit_meta;
use crate::state::State;

/// Largest request body: room for a batch of screenshots.
pub const MAX_BODY: usize = 32 * 1024 * 1024;

/// How long a purpose may be, in characters, once sanitised.
pub const PURPOSE_RANGE: std::ops::RangeInclusive<usize> = 8..=500;

/// A verified client with a live grant, and what its request said it was
/// for: the purpose, the question it serves, and the model running it.
#[derive(Clone, Debug)]
pub struct Caller {
	/// The client.
	pub client: Client,
	/// Its grant.
	pub grant: Grant,
	/// The purpose, question, and model headers of the request this caller
	/// was admitted on, signed with the rest.
	pub meta: RequestMeta,
}

impl Caller {
	/// The audit actor for this caller.
	pub fn actor(&self) -> Actor {
		Actor::Client {
			client: self.client.id,
			grant: self.grant.id,
		}
	}

	/// The question this request serves, when it named one that is the
	/// caller's to serve.
	pub fn question(&self) -> Option<QuestionId> {
		self.meta
			.question
			.as_deref()
			.and_then(|q| q.parse().ok())
	}
}

/// A signed request from a client with a live grant, and its JSON body.
pub struct Signed<T> {
	/// Who sent it.
	pub caller: Caller,
	/// What they sent.
	pub body: T,
}

/// A signed request from any paired client, approved or not.
pub struct SignedAny {
	/// Who sent it.
	pub client: Client,
}

/// A signed request with a raw body.
pub struct SignedBytes {
	/// Who sent it.
	pub caller: Caller,
	/// The body.
	pub body: Bytes,
	/// The request's query string.
	pub query: Option<String>,
}

fn verify(state: &State, parts: &Parts, body: &[u8]) -> Result<Client, Error> {
	let headers =
		SignedHeaders::from_lookup(|name| parts.headers.get(name).and_then(|v| v.to_str().ok()))?;
	let path = parts
		.uri
		.path_and_query()
		.map_or_else(|| parts.uri.path(), |p| p.as_str());
	let client = state
		.vault
		.store
		.client(headers.client)?
		.ok_or_else(|| Error::Unauthenticated("unknown client".into()))?;
	// Agents sign scheme V2 only, so their purpose travels inside the
	// signature; other clients may still present a V1 signature.
	state.verifier().verify(
		&client.public_key,
		&headers,
		Verify {
			method: parts.method.as_str(),
			path,
			body,
			allow_legacy: client.kind != ClientKind::Agent,
		},
		Timestamp::now(),
	)?;
	Ok(client)
}

fn admit(state: &State, client: Client, meta: RequestMeta, path: &str) -> Result<Caller, Error> {
	let now = Timestamp::now();
	let grant = state.vault.store.active_grant(client.id)?;
	let Some(grant) = grant else {
		return Err(Error::Unauthenticated(
			"this client has no live grant; ask the owner to approve it".into(),
		));
	};
	let caller = Caller {
		client,
		grant,
		meta,
	};
	let refused = caller
		.grant
		.check_live(now)
		.err()
		.or_else(|| state.limiter().admit(caller.grant.id, &caller.grant.limits, now).err());
	if let Some(denied) = refused {
		let (purpose, question, model) = audit_meta(&caller.meta);
		state.vault.store.audit(
			AuditEvent::new(caller.actor(), "request", "", Outcome::Denied { reason: denied })
				.meta(purpose.as_deref(), question, model.as_deref()),
		)?;
		return Err(Error::Forbidden(denied));
	}
	state.vault.store.touch_client(caller.client.id)?;
	check_purpose(state, &caller, path)?;
	Ok(caller)
}

/// Whether a path serves vault data, which an agent must justify with a
/// purpose. Query, search, series, durations, media, context, and the
/// profile claims are the reads that carry data out.
fn reads_data(path: &str) -> bool {
	path.starts_with("/v1/query")
		|| path.starts_with("/v1/media")
		|| path == quant_api::routes::CONTEXT
		|| path == quant_api::routes::MEMORY_PROFILE
}

fn check_purpose(state: &State, caller: &Caller, path: &str) -> Result<(), Error> {
	if caller.client.kind != ClientKind::Agent {
		return Ok(());
	}
	if !reads_data(path) {
		return Ok(());
	}
	let purpose = caller
		.meta
		.purpose
		.as_deref()
		.map(|p| quant_access::redact::sanitize(p).trim().to_owned());
	let valid = purpose.as_deref().is_some_and(|p| {
		let len = p.chars().count();
		PURPOSE_RANGE.contains(&len)
	});
	if !valid {
		let (purpose, question, model) = audit_meta(&caller.meta);
		let _ = state.vault.store.audit(
			AuditEvent::new(
				caller.actor(),
				"request",
				"",
				Outcome::Denied {
					reason: quant_access::Denied::PurposeRequired,
				},
			)
			.meta(purpose.as_deref(), question, model.as_deref()),
		);
		return Err(Error::PurposeRequired);
	}
	// A question named on a read must be the caller's own and still taking
	// reads; anything else answers as unknown, so one agent cannot write
	// into another's trail or probe for live ids.
	if let Some(question) = caller.meta.question.as_deref().and_then(|q| q.parse().ok()) {
		let ok = state.vault.store.question(question)?.is_some_and(|q| {
			q.client == caller.client.id
				&& matches!(
					q.status,
					quant_store::QuestionStatus::Open | quant_store::QuestionStatus::Answered
				)
		});
		if !ok {
			return Err(Error::Invalid("unknown question".into()));
		}
	}
	Ok(())
}

async fn read_body(body: axum::body::Body) -> Result<Bytes, Error> {
	axum::body::to_bytes(body, MAX_BODY)
		.await
		.map_err(|_| Error::Invalid("body too large".into()))
}

impl<T: DeserializeOwned + Send> FromRequest<Arc<State>> for Signed<T> {
	type Rejection = Error;

	async fn from_request(request: Request, state: &Arc<State>) -> Result<Self, Self::Rejection> {
		let (parts, body) = request.into_parts();
		let bytes = read_body(body).await?;
		let client = verify(state, &parts, &bytes)?;
		let meta = meta_of(&parts);
		let caller = admit(state, client, meta, parts.uri.path())?;
		let json: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
		let body = serde_json::from_slice(json).map_err(|e| Error::Invalid(e.to_string()))?;
		Ok(Signed { caller, body })
	}
}

impl FromRequest<Arc<State>> for SignedAny {
	type Rejection = Error;

	async fn from_request(request: Request, state: &Arc<State>) -> Result<Self, Self::Rejection> {
		let (parts, body) = request.into_parts();
		let bytes = read_body(body).await?;
		let client = verify(state, &parts, &bytes)?;
		Ok(SignedAny { client })
	}
}

impl FromRequest<Arc<State>> for SignedBytes {
	type Rejection = Error;

	async fn from_request(request: Request, state: &Arc<State>) -> Result<Self, Self::Rejection> {
		let (parts, body) = request.into_parts();
		let bytes = read_body(body).await?;
		let client = verify(state, &parts, &bytes)?;
		let meta = meta_of(&parts);
		let caller = admit(state, client, meta, parts.uri.path())?;
		Ok(SignedBytes {
			caller,
			body: bytes,
			query: parts.uri.query().map(ToOwned::to_owned),
		})
	}
}

/// The request meta from a request's headers. The signature has already
/// covered these values when `verify` accepted the request.
fn meta_of(parts: &Parts) -> RequestMeta {
	let read = |name: &str| {
		parts
			.headers
			.get(name)
			.and_then(|v| v.to_str().ok())
			.map(str::trim)
			.filter(|v| !v.is_empty())
			.map(ToOwned::to_owned)
	};
	RequestMeta {
		purpose: read(quant_access::auth::HEADER_PURPOSE),
		question: read(quant_access::auth::HEADER_QUESTION),
		model: read(quant_access::auth::HEADER_MODEL),
	}
}

/// A request carrying a live owner session.
pub struct Owner {
	/// The session token.
	pub token: String,
	/// How this session was opened: with the passphrase, or with an owner
	/// device.
	pub via: SessionSource,
}

impl FromRequestParts<Arc<State>> for Owner {
	type Rejection = Error;

	#[allow(clippy::unused_async_trait_impl)]
	async fn from_request_parts(
		parts: &mut Parts,
		state: &Arc<State>,
	) -> Result<Self, Self::Rejection> {
		let token = parts
			.headers
			.get(OWNER_HEADER)
			.and_then(|v| v.to_str().ok())
			.ok_or_else(|| Error::Unauthenticated("owner session required; unlock first".into()))?;
		match state.owner().check(token, Timestamp::now()) {
			Some(via) => {
				Ok(Owner {
					token: token.to_owned(),
					via,
				})
			}
			None => {
				Err(Error::Unauthenticated(
					"owner session expired or invalid; unlock again".into(),
				))
			}
		}
	}
}
