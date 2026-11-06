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
	) -> SignedHeaders {
		let mut nonce = [0u8; 16];
		getrandom::fill(&mut nonce).expect("the OS random source is available");
		let nonce = hex::encode(nonce);
		let timestamp = now.as_second();
		let message = canonical(method, path, timestamp, &nonce, body);
		let signature = URL_SAFE_NO_PAD.encode(self.0.sign(&message).to_bytes());
		SignedHeaders {
			client,
			timestamp,
			nonce,
			signature,
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
}

impl SignedHeaders {
	/// The headers as name/value pairs.
	pub fn pairs(&self) -> [(&'static str, String); 4] {
		[
			(HEADER_CLIENT, self.client.to_string()),
			(HEADER_TIMESTAMP, self.timestamp.to_string()),
			(HEADER_NONCE, self.nonce.clone()),
			(HEADER_SIGNATURE, self.signature.clone()),
		]
	}

	/// Read the headers back from a lookup function.
	pub fn from_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, AccessError> {
		let missing = |name: &str| AccessError::Unauthenticated(format!("missing {name}"));
		let client = get(HEADER_CLIENT).ok_or_else(|| missing(HEADER_CLIENT))?;
		let timestamp = get(HEADER_TIMESTAMP).ok_or_else(|| missing(HEADER_TIMESTAMP))?;
		let nonce = get(HEADER_NONCE).ok_or_else(|| missing(HEADER_NONCE))?;
		let signature = get(HEADER_SIGNATURE).ok_or_else(|| missing(HEADER_SIGNATURE))?;
		Ok(Self {
			client: client
				.parse()
				.map_err(|_| AccessError::Unauthenticated("bad client id".into()))?,
			timestamp: timestamp
				.parse()
				.map_err(|_| AccessError::Unauthenticated("bad timestamp".into()))?,
			nonce: nonce.to_owned(),
			signature: signature.to_owned(),
		})
	}
}

/// The bytes a request signature covers. The body is folded in by hash so
/// large uploads are hashed once.
pub fn canonical(method: &str, path: &str, timestamp: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
	let body_hash = blake3::hash(body).to_hex();
	format!("QUANT-SIG-V1\n{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}").into_bytes()
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
	/// and only once.
	pub fn verify(
		&mut self,
		key: &PublicKey,
		headers: &SignedHeaders,
		method: &str,
		path: &str,
		body: &[u8],
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
		let message = canonical(method, path, headers.timestamp, &headers.nonce, body);
		key.0
			.verify(&message, &signature)
			.map_err(|_| unauthenticated("signature does not match"))?;

		// Only remember nonces of requests that verified, so junk cannot fill
		// the table, and forget them once their timestamp could no longer pass.
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

	#[test]
	fn accepts_a_valid_signature_once() {
		let (key, client, now) = setup();
		let headers = key.sign(client, "POST", "/v1/query", b"{}", now);
		let mut verifier = RequestVerifier::new();
		verifier
			.verify(&key.public(), &headers, "POST", "/v1/query", b"{}", now)
			.unwrap();
		let replay = verifier.verify(&key.public(), &headers, "POST", "/v1/query", b"{}", now);
		assert!(matches!(replay, Err(AccessError::Unauthenticated(why)) if why.contains("replay")));
	}

	#[test]
	fn rejects_tampering() {
		let (key, client, now) = setup();
		let headers = key.sign(client, "POST", "/v1/query", b"{}", now);
		let mut verifier = RequestVerifier::new();
		for (method, path, body) in [
			("GET", "/v1/query", b"{}".as_slice()),
			("POST", "/v1/admin", b"{}".as_slice()),
			("POST", "/v1/query", b"{\"x\":1}".as_slice()),
		] {
			assert!(
				verifier
					.verify(&key.public(), &headers, method, path, body, now)
					.is_err()
			);
		}
	}

	#[test]
	fn rejects_other_keys() {
		let (key, client, now) = setup();
		let headers = key.sign(client, "GET", "/", b"", now);
		let other = ClientKey::generate();
		assert!(
			RequestVerifier::new()
				.verify(&other.public(), &headers, "GET", "/", b"", now)
				.is_err()
		);
	}

	#[test]
	fn rejects_stale_timestamps() {
		let (key, client, now) = setup();
		let headers = key.sign(client, "GET", "/", b"", now - SignedDuration::from_mins(5));
		assert!(
			RequestVerifier::new()
				.verify(&key.public(), &headers, "GET", "/", b"", now)
				.is_err()
		);
	}

	#[test]
	fn keys_and_headers_round_trip() {
		let (key, client, now) = setup();
		let public: PublicKey = key.public().to_string().parse().unwrap();
		assert_eq!(public, key.public());
		assert_eq!(ClientKey::from_seed(&key.seed()).public(), key.public());

		let headers = key.sign(client, "GET", "/", b"", now);
		let pairs = headers.pairs();
		let back = SignedHeaders::from_lookup(|name| {
			pairs
				.iter()
				.find(|(n, _)| *n == name)
				.map(|(_, v)| v.as_str())
		})
		.unwrap();
		assert_eq!(back, headers);
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
