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
use quant_access::auth::SignedHeaders;
use quant_api::routes::OWNER_HEADER;
use quant_store::Client;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::state::State;

/// Largest request body: room for a batch of screenshots.
pub const MAX_BODY: usize = 32 * 1024 * 1024;

/// A verified client with a live grant.
#[derive(Clone, Debug)]
pub struct Caller {
	/// The client.
	pub client: Client,
	/// Its grant.
	pub grant: Grant,
}

impl Caller {
	/// The audit actor for this caller.
	pub fn actor(&self) -> Actor {
		Actor::Client {
			client: self.client.id,
			grant: self.grant.id,
		}
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
	state.verifier().verify(
		&client.public_key,
		&headers,
		parts.method.as_str(),
		path,
		body,
		Timestamp::now(),
	)?;
	Ok(client)
}

fn admit(state: &State, client: Client) -> Result<Caller, Error> {
	let now = Timestamp::now();
	let grant = state.vault.store.active_grant(client.id)?;
	let Some(grant) = grant else {
		return Err(Error::Unauthenticated(
			"this client has no live grant; ask the owner to approve it".into(),
		));
	};
	let refused = grant
		.check_live(now)
		.err()
		.or_else(|| state.limiter().admit(grant.id, &grant.limits, now).err());
	if let Some(denied) = refused {
		state.vault.store.audit(AuditEvent::new(
			Actor::Client {
				client: client.id,
				grant: grant.id,
			},
			"request",
			"",
			Outcome::Denied { reason: denied },
		))?;
		return Err(Error::Forbidden(denied));
	}
	state.vault.store.touch_client(client.id)?;
	Ok(Caller { client, grant })
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
		let caller = admit(state, client)?;
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
		let caller = admit(state, client)?;
		Ok(SignedBytes {
			caller,
			body: bytes,
			query: parts.uri.query().map(ToOwned::to_owned),
		})
	}
}

/// A request carrying a live owner session.
pub struct Owner {
	/// The session token.
	pub token: String,
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
		if state.owner().check(token, Timestamp::now()) {
			Ok(Owner {
				token: token.to_owned(),
			})
		} else {
			Err(Error::Unauthenticated(
				"owner session expired or invalid; unlock again".into(),
			))
		}
	}
}
