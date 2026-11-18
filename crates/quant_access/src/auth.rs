//! Ed25519 request signing: each client signs every request with its own
//! key, and the vault refuses stale, altered, or replayed requests.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::Verifier as _;
use ed25519_dalek::VerifyingKey;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::ClientId;
use serde::Deserialize;
use serde::Serialize;

use crate::AccessError;

/// Header carrying the client id.
pub const HEADER_CLIENT: &str = "x-quant-client";
/// Header carrying the unix time the request was signed, in seconds.
pub const HEADER_TIMESTAMP: &str = "x-quant-timestamp";
/// Header carrying a single-use random nonce.
pub const HEADER_NONCE: &str = "x-quant-nonce";
/// Header carrying the request signature.
pub const HEADER_SIGNATURE: &str = "x-quant-signature";
/// Header carrying why an agent is reading: its purpose, in its own words.
/// Agents must send it on every read route; the signature covers it, so it
/// cannot be stripped or rewritten in flight.
pub const HEADER_PURPOSE: &str = "x-quant-purpose";
/// Header carrying the question a read belongs to, as
/// [`quant_api`](crate) ids it. Optional.
pub const HEADER_QUESTION: &str = "x-quant-question";
/// Header carrying the model actually running this session, when that
/// differs from the one given at pairing. Optional.
pub const HEADER_MODEL: &str = "x-quant-model";

/// Why a request is being made, as the client states it. The purpose is
/// required of agents on every read route and is signed with the rest of
/// the request, then recorded in the audit log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestMeta {
	/// Why the data is needed, 8 to 500 characters once sanitised.
	pub purpose: Option<String>,
	/// The question this read belongs to.
	pub question: Option<String>,
	/// The model actually running this session.
	pub model: Option<String>,
}

impl RequestMeta {
	/// A meta carrying only a purpose.
	pub fn purpose(purpose: impl Into<String>) -> Self {
		Self {
			purpose: Some(purpose.into()),
			question: None,
			model: None,
		}
	}

	/// Serve the question with this id.
	#[must_use]
	pub fn with_question(mut self, question: quant_core::QuestionId) -> Self {
		self.question = Some(question.to_string());
		self
	}

	/// Name the model running this session.
	#[must_use]
	pub fn with_model(mut self, model: impl Into<String>) -> Self {
		self.model = Some(model.into());
		self
	}

	/// Whether any field is set.
	pub fn is_empty(&self) -> bool {
		self.purpose.is_none() && self.question.is_none() && self.model.is_none()
	}

	fn values(&self) -> [&str; 3] {
		[
			self.purpose.as_deref().unwrap_or(""),
			self.question.as_deref().unwrap_or(""),
			self.model.as_deref().unwrap_or(""),
		]
	}
}

/// How far a request's timestamp may drift from the vault's clock.
pub const MAX_SKEW: SignedDuration = SignedDuration::from_secs(60);

/// A client's private signing key. It never leaves the client: the vault
/// only ever sees the public half, so a copy of the vault's database cannot
/// impersonate anyone.
pub struct ClientKey(SigningKey);

impl ClientKey {
	/// A fresh random key.
	pub fn generate() -> Self {
		let mut seed = [0u8; 32];
		getrandom::fill(&mut seed).expect("the OS random source is available");
		let key = Self(SigningKey::from_bytes(&seed));
		seed.fill(0);
		key
	}

	/// Restore a key from its 32-byte seed.
	pub fn from_seed(seed: &[u8; 32]) -> Self {
		Self(SigningKey::from_bytes(seed))
	}

	/// The 32-byte seed, for storing in a keychain.
	pub fn seed(&self) -> [u8; 32] {
		self.0.to_bytes()
	}

	/// The public half.
	pub fn public(&self) -> PublicKey {
		PublicKey(self.0.verifying_key())
	}

	/// Sign a request.
	pub fn sign(
		&self,
		client: ClientId,
		method: &str,
		path: &str,
		body: &[u8],
		now: Timestamp,
		meta: &RequestMeta,
	) -> SignedHeaders {
		let mut nonce = [0u8; 16];
		getrandom::fill(&mut nonce).expect("the OS random source is available");
		let nonce = hex::encode(nonce);
		let timestamp = now.as_second();
		let message = canonical(method, path, timestamp, &nonce, body, meta);
		let signature = URL_SAFE_NO_PAD.encode(self.0.sign(&message).to_bytes());
		SignedHeaders {
			client,
			timestamp,
			nonce,
			signature,
			meta: meta.clone(),
		}
	}
}

impl fmt::Debug for ClientKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("ClientKey").field(&self.public()).finish()
	}
}

/// A client's public key, shown as unpadded URL-safe base64.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
	/// Wrap raw key bytes.
	pub fn from_bytes(bytes: &[u8; 32]) -> Self {
		Self(VerifyingKey::from_bytes(bytes).expect("a public key of 32 bytes is valid"))
	}

	/// A short code people compare when approving a pairing, such as
	/// `K7QD-M2XA`. Both sides derive it from the key, so a mismatch means
	/// someone else's request.
	pub fn pairing_code(&self) -> String {
		const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
		let hash = blake3::hash(self.0.as_bytes());
		let code: String = hash.as_bytes()[..8]
			.iter()
			.map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
			.collect();
		format!("{}-{}", &code[..4], &code[4..])
	}

	/// Raw key bytes.
	pub fn as_bytes(&self) -> &[u8; 32] {
		self.0.as_bytes()
	}
}

impl fmt::Display for PublicKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&URL_SAFE_NO_PAD.encode(self.0.as_bytes()))
	}
}

impl fmt::Debug for PublicKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "PublicKey({self})")
	}
}

impl FromStr for PublicKey {
	type Err = AccessError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let bytes = URL_SAFE_NO_PAD
			.decode(s)
			.map_err(|_| AccessError::InvalidKey)?;
		let bytes: [u8; 32] = bytes.try_into().map_err(|_| AccessError::InvalidKey)?;
		VerifyingKey::from_bytes(&bytes)
			.map(Self)
			.map_err(|_| AccessError::InvalidKey)
	}
}

impl TryFrom<String> for PublicKey {
	type Error = AccessError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		value.parse()
	}
}

impl From<PublicKey> for String {
	fn from(value: PublicKey) -> Self {
		value.to_string()
	}
}

/// The signature headers a client attaches to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHeaders {
	/// The signing client.
	pub client: ClientId,
	/// Unix seconds when signed.
	pub timestamp: i64,
	/// Single-use hex nonce.
	pub nonce: String,
	/// Base64 Ed25519 signature over the canonical request.
	pub signature: String,
	/// The purpose, question, and model headers, signed with the rest.
	pub meta: RequestMeta,
}

impl SignedHeaders {
	/// The headers as name/value pairs.
	pub fn pairs(&self) -> Vec<(&'static str, String)> {
		let mut pairs = vec![
			(HEADER_CLIENT, self.client.to_string()),
			(HEADER_TIMESTAMP, self.timestamp.to_string()),
			(HEADER_NONCE, self.nonce.clone()),
			(HEADER_SIGNATURE, self.signature.clone()),
		];
		let mut push = |name: &'static str, value: &Option<String>| {
			if let Some(value) = value
				&& !value.is_empty()
			{
				pairs.push((name, value.clone()));
			}
		};
		push(HEADER_PURPOSE, &self.meta.purpose);
		push(HEADER_QUESTION, &self.meta.question);
		push(HEADER_MODEL, &self.meta.model);
		pairs
	}

	/// Read the headers back from a lookup function.
	pub fn from_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, AccessError> {
		let missing = |name: &str| AccessError::Unauthenticated(format!("missing {name}"));
		let client = get(HEADER_CLIENT).ok_or_else(|| missing(HEADER_CLIENT))?;
		let timestamp = get(HEADER_TIMESTAMP).ok_or_else(|| missing(HEADER_TIMESTAMP))?;
		let nonce = get(HEADER_NONCE).ok_or_else(|| missing(HEADER_NONCE))?;
		let signature = get(HEADER_SIGNATURE).ok_or_else(|| missing(HEADER_SIGNATURE))?;
		let optional = |name: &str| {
			get(name)
				.map(str::trim)
				.filter(|value| !value.is_empty())
				.map(ToOwned::to_owned)
		};
		Ok(Self {
			client: client
				.parse()
				.map_err(|_| AccessError::Unauthenticated("bad client id".into()))?,
			timestamp: timestamp
				.parse()
				.map_err(|_| AccessError::Unauthenticated("bad timestamp".into()))?,
			nonce: nonce.to_owned(),
			signature: signature.to_owned(),
			meta: RequestMeta {
				purpose: optional(HEADER_PURPOSE),
				question: optional(HEADER_QUESTION),
				model: optional(HEADER_MODEL),
			},
		})
	}
}

/// The bytes a request signature covers, folded into scheme V2: the
/// purpose, question, and model headers are signed with the rest, so
/// nothing in flight can strip the reason for a read. The body is folded
/// in by hash so large uploads are hashed once.
pub fn canonical(
	method: &str,
	path: &str,
	timestamp: i64,
	nonce: &str,
	body: &[u8],
	meta: &RequestMeta,
) -> Vec<u8> {
	let body_hash = blake3::hash(body).to_hex();
	let [purpose, question, model] = meta.values();
	format!("QUANT-SIG-V2\n{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}\n{purpose}\n{question}\n{model}")
		.into_bytes()
}

/// The bytes a scheme V1 signature covered: no purpose headers. Kept only
/// so clients that predate purposes — the CLI, devices, the app — keep
/// working; an agent must sign V2.
fn canonical_v1(method: &str, path: &str, timestamp: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
	let body_hash = blake3::hash(body).to_hex();
	format!("QUANT-SIG-V1\n{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}").into_bytes()
}

/// What is being verified: the request's method, path, and body, and
/// whether a signature from before purposes existed may still stand.
#[derive(Clone, Copy)]
pub struct Verify<'a> {
	/// The HTTP method.
	pub method: &'a str,
	/// The path with its query string, as signed.
	pub path: &'a str,
	/// The body bytes.
	pub body: &'a [u8],
	/// Whether a V1 signature is acceptable: true for clients that never
	/// send purposes, false for agents.
	pub allow_legacy: bool,
}

/// Checks request signatures and refuses replays.
#[derive(Debug, Default)]
pub struct RequestVerifier {
	seen: HashMap<String, i64>,
}

impl RequestVerifier {
	/// A verifier with no remembered nonces.
	pub fn new() -> Self {
		Self::default()
	}

	/// Verify `headers` were made by `key` for this exact request, recently,
	/// and only once. Scheme V2 signatures cover the purpose headers; the
	/// V1 fallback is allowed only for clients that never send purposes
	/// (the CLI, devices, the app), never for agents.
	pub fn verify(
		&mut self,
		key: &PublicKey,
		headers: &SignedHeaders,
		request: Verify<'_>,
		now: Timestamp,
	) -> Result<(), AccessError> {
		let unauthenticated = |why: &str| AccessError::Unauthenticated(why.to_owned());
		let now = now.as_second();
		if (now - headers.timestamp).abs() > MAX_SKEW.as_secs() {
			return Err(unauthenticated("timestamp outside the allowed window"));
		}
		if headers.nonce.len() < 16 || headers.nonce.len() > 64 {
			return Err(unauthenticated("bad nonce"));
		}
		let signature = URL_SAFE_NO_PAD
			.decode(&headers.signature)
			.ok()
			.and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
			.map(|bytes| Signature::from_bytes(&bytes))
			.ok_or_else(|| unauthenticated("bad signature encoding"))?;
		let Verify {
			method,
			path,
			body,
			allow_legacy,
		} = request;
		let message = canonical(
			method,
			path,
			headers.timestamp,
			&headers.nonce,
			body,
			&headers.meta,
		);
		let verified = key.0.verify(&message, &signature).is_ok()
			|| allow_legacy
				&& key
					.0
					.verify(
						&canonical_v1(method, path, headers.timestamp, &headers.nonce, body),
						&signature,
					)
					.is_ok();
		if !verified {
			return Err(unauthenticated("signature does not match"));
		}

		// Only remember nonces of requests that verified, so junk cannot
		// fill the table, and forget them once their timestamp could no
		// longer pass.
		self.seen
			.retain(|_, at| (now - *at).abs() <= MAX_SKEW.as_secs());
		let replay_key = format!("{}:{}", headers.client, headers.nonce);
		if self.seen.insert(replay_key, headers.timestamp).is_some() {
			return Err(unauthenticated("replayed request"));
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn setup() -> (ClientKey, ClientId, Timestamp) {
		(ClientKey::generate(), ClientId::new(), Timestamp::now())
	}

	fn sign(
		key: &ClientKey,
		client: ClientId,
		method: &str,
		path: &str,
		body: &[u8],
		now: Timestamp,
		meta: &RequestMeta,
	) -> SignedHeaders {
		key.sign(client, method, path, body, now, meta)
	}

	#[test]
	fn accepts_a_valid_signature_once() {
		let (key, client, now) = setup();
		let meta = RequestMeta::purpose("answering the owner's question about sleep");
		let headers = sign(&key, client, "POST", "/v1/query", b"{}", now, &meta);
		let mut verifier = RequestVerifier::new();
		verifier
			.verify(
				&key.public(),
				&headers,
				Verify {
					method: "POST",
					path: "/v1/query",
					body: b"{}",
					allow_legacy: false,
				},
				now,
			)
			.unwrap();
		let replay =
			verifier.verify(
				&key.public(),
				&headers,
				Verify {
					method: "POST",
					path: "/v1/query",
					body: b"{}",
					allow_legacy: false,
				},
				now,
			);
		assert!(matches!(replay, Err(AccessError::Unauthenticated(why)) if why.contains("replay")));
	}

	#[test]
	fn rejects_tampering() {
		let (key, client, now) = setup();
		let meta = RequestMeta::purpose("checking tamper detection works");
		let headers = sign(&key, client, "POST", "/v1/query", b"{}", now, &meta);
		let mut verifier = RequestVerifier::new();
		for (method, path, body) in [
			("GET", "/v1/query", b"{}".as_slice()),
			("POST", "/v1/admin", b"{}".as_slice()),
			("POST", "/v1/query", b"{\"x\":1}".as_slice()),
		] {
			assert!(
				verifier
					.verify(
						&key.public(),
						&headers,
						Verify {
							method,
							path,
							body,
							allow_legacy: false,
						},
						now,
					)
					.is_err()
			);
		}
	}

	#[test]
	fn the_purpose_headers_are_part_of_the_signature() {
		let (key, client, now) = setup();
		let meta = RequestMeta {
			purpose: Some("finding when the owner focuses best".into()),
			question: Some("ques_0123456789abcdef".into()),
			model: Some("claude-opus-5-5".into()),
		};
		let headers = sign(&key, client, "POST", "/v1/query/search", b"{}", now, &meta);
		let mut verifier = RequestVerifier::new();
		// Stripping or rewriting the purpose fails the signature, legacy or
		// not: an agent cannot read without its stated reason.
		for rewrite in [
			RequestMeta::default(),
			RequestMeta::purpose("no reason given here"),
			RequestMeta {
				question: Some("ques_ffffffffffffffff".into()),
				..RequestMeta::purpose("finding when the owner focuses best")
			},
		] {
			let mut altered = headers.clone();
			altered.meta = rewrite;
			assert!(
				verifier
					.verify(
				&key.public(),
				&altered,
				Verify {
					method: "POST",
					path: "/v1/query/search",
					body: b"{}",
					allow_legacy: true,
				},
				now,
			)
					.is_err()
			);
		}
		// And the untampered request still verifies.
		verifier
			.verify(
				&key.public(),
				&headers,
				Verify {
					method: "POST",
					path: "/v1/query/search",
					body: b"{}",
					allow_legacy: true,
				},
				now,
			)
			.unwrap();
	}

	#[test]
	fn v1_signatures_still_work_where_purposes_are_not_required() {
		let (key, client, now) = setup();
		let mut headers = sign(&key, client, "GET", "/v1/me", b"", now, &RequestMeta::default());
		headers.signature = URL_SAFE_NO_PAD.encode(
			key.0
				.sign(&canonical_v1("GET", "/v1/me", headers.timestamp, &headers.nonce, b""))
				.to_bytes(),
		);
		let mut verifier = RequestVerifier::new();
		verifier
			.verify(
				&key.public(),
				&headers,
				Verify {
					method: "GET",
					path: "/v1/me",
					body: b"",
					allow_legacy: true,
				},
				now,
			)
			.unwrap();
		// An agent may not fall back to V1: it must sign its purpose.
		assert!(
			verifier
				.verify(
				&key.public(),
				&headers,
				Verify {
					method: "GET",
					path: "/v1/me",
					body: b"",
					allow_legacy: false,
				},
				now,
			)
				.is_err()
		);
	}

	#[test]
	fn rejects_other_keys() {
		let (key, client, now) = setup();
		let headers = sign(&key, client, "GET", "/", b"", now, &RequestMeta::default());
		let other = ClientKey::generate();
		assert!(
			RequestVerifier::new()
				.verify(
				&other.public(),
				&headers,
				Verify {
					method: "GET",
					path: "/",
					body: b"",
					allow_legacy: false,
				},
				now,
			)
				.is_err()
		);
	}

	#[test]
	fn rejects_stale_timestamps() {
		let (key, client, now) = setup();
		let headers = sign(
			&key,
			client,
			"GET",
			"/",
			b"",
			now - SignedDuration::from_mins(5),
			&RequestMeta::default(),
		);
		assert!(
			RequestVerifier::new()
				.verify(
				&key.public(),
				&headers,
				Verify {
					method: "GET",
					path: "/",
					body: b"",
					allow_legacy: false,
				},
				now,
			)
				.is_err()
		);
	}

	#[test]
	fn keys_and_headers_round_trip() {
		let (key, client, now) = setup();
		let public: PublicKey = key.public().to_string().parse().unwrap();
		assert_eq!(public, key.public());
		assert_eq!(ClientKey::from_seed(&key.seed()).public(), key.public());

		let meta = RequestMeta {
			purpose: Some("summarising the owner's week".into()),
			question: None,
			model: Some("claude-opus-5-5".into()),
		};
		let headers = sign(&key, client, "GET", "/", b"", now, &meta);
		let pairs = headers.pairs();
		let back = SignedHeaders::from_lookup(|name| {
			pairs
				.iter()
				.find(|(n, _)| *n == name)
				.map(|(_, v)| v.as_str())
		})
		.unwrap();
		assert_eq!(back, headers);
		assert!(pairs.len() > 4);
	}

	#[test]
	fn pairing_codes_are_stable_and_distinct() {
		let a = ClientKey::generate().public();
		let b = ClientKey::generate().public();
		assert_eq!(a.pairing_code(), a.pairing_code());
		assert_ne!(a.pairing_code(), b.pairing_code());
		assert_eq!(a.pairing_code().len(), 9);
	}
}
